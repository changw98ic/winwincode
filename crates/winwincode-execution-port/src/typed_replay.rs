//! Typed adapters from the generated stream messages to the shared replay
//! state machine.
//!
//! This module only validates wire shape, derives a stable stream key, and
//! retains the canonical encoded message. Runtime frames use the nested event
//! record as their semantic digest so transport envelope ids do not split
//! duplicate handling. Lease authority, product state, and persistence remain
//! caller-owned concerns of [`crate::replay`].

use std::fmt;

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{SchemaVersion, SessionIdentity};

use crate::generated::{
    ArtifactAckMessage, ArtifactAckMessageKind, ArtifactChunkMessage, ArtifactChunkMessageKind,
    ArtifactOpenMessage, ArtifactOpenMessageKind, ExecutionLeaseStamp, ExecutionPortMessage,
    LeaseWriteStatus, ModelAckMessage, ModelAckMessageKind, ModelChunkMessage,
    ModelChunkMessageKind, ModelOpenMessage, ModelOpenMessageKind, RuntimeAckMessage,
    RuntimeAckMessageKind, RuntimeEventMessage, RuntimeEventMessageKind,
    RuntimeReplayRequestMessage, RuntimeReplayRequestMessageKind,
};
use crate::replay::{
    ReplayAcknowledgement, ReplayAcknowledgementStatus, ReplayFrame, ReplaySequence,
    ReplayStreamKey,
};
use crate::runtime_replay::{
    runtime_ack_stream_key, runtime_event_stream_key, runtime_replay_stream_key,
};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// The three lease-scoped streams that use the replay state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReplayStreamKind {
    Runtime,
    Artifact,
    Model,
}

/// A generated stream frame mapped to the transport-neutral replay shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedReplayFrame {
    /// The generated stream family represented by `frame`.
    pub kind: ReplayStreamKind,
    /// The canonical lease/resource stream key.
    pub stream: ReplayStreamKey,
    /// The original canonical JSON frame and its digest.
    pub frame: ReplayFrame,
}

/// A generated acknowledgement mapped to the transport-neutral replay shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedReplayAcknowledgement {
    /// The generated stream family represented by `acknowledgement`.
    pub kind: ReplayStreamKind,
    /// The canonical lease/resource stream key.
    pub stream: ReplayStreamKey,
    /// The generic acknowledgement cursor and status.
    pub acknowledgement: ReplayAcknowledgement,
}

/// A stream key extracted from any runtime, artifact, or model stream message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedReplayStream {
    /// The generated stream family represented by `stream`.
    pub kind: ReplayStreamKind,
    /// The canonical lease/resource stream key.
    pub stream: ReplayStreamKey,
}

/// Shape failures found before a replay store or authority is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedReplayMappingError {
    /// The message is not one of the runtime, artifact, or model stream types.
    UnsupportedMessage,
    /// A stream control message carries no sequenceable frame.
    UnsequencedMessage,
    /// The generated discriminator does not match the expected message type.
    WrongKind,
    /// The generated schema version is not supported by this adapter.
    UnsupportedSchema,
    /// A required identity or message identifier is empty.
    EmptyIdentity,
    /// A frame event identifier is empty.
    EmptyEventId,
    /// A sequence is zero or negative.
    InvalidSequence,
    /// A sequence exceeds the JSON-safe wire range.
    SequenceOutOfRange,
    /// A lease attempt is not positive.
    InvalidAttempt,
    /// A fencing token is not canonical decimal text.
    InvalidFencingToken,
    /// The duplicated `WorkerSession` values differ.
    WorkerSessionMismatch,
    /// The duplicated `CodexThread` values differ.
    CodexThreadMismatch,
    /// A negative acknowledgement was supplied.
    NegativeAcknowledgement,
    /// A gap acknowledgement omitted its replay start.
    GapReplayMissing,
    /// A gap acknowledgement did not start at ack plus one.
    GapReplayMismatch,
    /// A non-gap acknowledgement supplied a replay start.
    UnexpectedReplayStart,
    /// Canonical JSON encoding failed.
    Serialization,
}

impl fmt::Display for TypedReplayMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedMessage => "message is not a replay stream message",
            Self::UnsequencedMessage => "stream declaration has no replay sequence",
            Self::WrongKind => "replay stream message kind is invalid",
            Self::UnsupportedSchema => "replay stream message schema is unsupported",
            Self::EmptyIdentity => "replay stream identity is empty",
            Self::EmptyEventId => "replay frame event id is empty",
            Self::InvalidSequence => "replay frame sequence is invalid",
            Self::SequenceOutOfRange => "replay sequence is outside the safe range",
            Self::InvalidAttempt => "replay lease attempt is invalid",
            Self::InvalidFencingToken => "replay lease fencing token is invalid",
            Self::WorkerSessionMismatch => "replay WorkerSession identities differ",
            Self::CodexThreadMismatch => "replay CodexThread identities differ",
            Self::NegativeAcknowledgement => "replay acknowledgement is negative",
            Self::GapReplayMissing => "gap acknowledgement has no replay start",
            Self::GapReplayMismatch => "gap acknowledgement replay start is not ack plus one",
            Self::UnexpectedReplayStart => "non-gap acknowledgement has a replay start",
            Self::Serialization => "replay stream message cannot be encoded canonically",
        })
    }
}

impl std::error::Error for TypedReplayMappingError {}

/// Maps one generated runtime, artifact, or model data message to a generic
/// [`ReplayFrame`].  Stream declarations, replay requests, and acknowledgements
/// are intentionally not accepted here because they do not carry a frame
/// sequence.
///
/// # Errors
///
/// Returns a mapping error when the message is not a sequenced stream frame or
/// when its generated identity, sequence, schema, or canonical encoding is
/// invalid.
pub fn frame_from_message(
    message: &ExecutionPortMessage,
) -> Result<TypedReplayFrame, TypedReplayMappingError> {
    match message {
        ExecutionPortMessage::RuntimeEventMessage(message) => frame_from_runtime_event(message),
        ExecutionPortMessage::ArtifactChunkMessage(message) => frame_from_artifact_chunk(message),
        ExecutionPortMessage::ModelChunkMessage(message) => frame_from_model_chunk(message),
        ExecutionPortMessage::RuntimeAckMessage(_)
        | ExecutionPortMessage::RuntimeReplayRequestMessage(_)
        | ExecutionPortMessage::ArtifactOpenMessage(_)
        | ExecutionPortMessage::ArtifactAckMessage(_)
        | ExecutionPortMessage::ModelOpenMessage(_)
        | ExecutionPortMessage::ModelAckMessage(_) => {
            Err(TypedReplayMappingError::UnsequencedMessage)
        }
        _ => Err(TypedReplayMappingError::UnsupportedMessage),
    }
}

/// Maps one generated runtime, artifact, or model acknowledgement to the
/// generic acknowledgement cursor and status used by the replay state machine.
///
/// # Errors
///
/// Returns a mapping error when the message is not a stream acknowledgement or
/// when its identity, cursor, schema, or gap replay hint is invalid.
pub fn acknowledgement_from_message(
    message: &ExecutionPortMessage,
) -> Result<TypedReplayAcknowledgement, TypedReplayMappingError> {
    match message {
        ExecutionPortMessage::RuntimeAckMessage(message) => acknowledgement_from_runtime(message),
        ExecutionPortMessage::ArtifactAckMessage(message) => acknowledgement_from_artifact(message),
        ExecutionPortMessage::ModelAckMessage(message) => acknowledgement_from_model(message),
        _ => Err(TypedReplayMappingError::UnsupportedMessage),
    }
}

/// Derives the same stream key for an open, frame, acknowledgement, or replay
/// request message.  This function creates no lease authority and performs no
/// product-state lookup.
///
/// # Errors
///
/// Returns a mapping error when the message is outside the supported stream
/// families or contains an invalid generated identity.
pub fn stream_key_from_message(
    message: &ExecutionPortMessage,
) -> Result<TypedReplayStream, TypedReplayMappingError> {
    match message {
        ExecutionPortMessage::RuntimeEventMessage(message) => {
            validate_runtime_event(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Runtime,
                stream: runtime_event_stream_key(message),
            })
        }
        ExecutionPortMessage::RuntimeAckMessage(message) => {
            validate_runtime_ack(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Runtime,
                stream: runtime_ack_stream_key(message),
            })
        }
        ExecutionPortMessage::RuntimeReplayRequestMessage(message) => {
            validate_runtime_replay_request(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Runtime,
                stream: runtime_replay_stream_key(message),
            })
        }
        ExecutionPortMessage::ArtifactOpenMessage(message) => {
            validate_artifact_open(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Artifact,
                stream: artifact_stream_key(
                    &message.lease,
                    &message.worker_session_id,
                    &message.session_identity,
                    &message.artifact.artifact_id.0,
                ),
            })
        }
        ExecutionPortMessage::ArtifactChunkMessage(message) => {
            validate_artifact_chunk(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Artifact,
                stream: artifact_stream_key(
                    &message.lease,
                    &message.worker_session_id,
                    &message.session_identity,
                    &message.artifact_id.0,
                ),
            })
        }
        ExecutionPortMessage::ArtifactAckMessage(message) => {
            validate_artifact_ack(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Artifact,
                stream: artifact_stream_key(
                    &message.lease,
                    &message.worker_session_id,
                    &message.session_identity,
                    &message.artifact_id.0,
                ),
            })
        }
        ExecutionPortMessage::ModelOpenMessage(message) => {
            validate_model_open(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Model,
                stream: model_stream_key(
                    &message.lease,
                    &message.worker_session_id,
                    &message.session_identity,
                    &message.model_exchange_id.0,
                ),
            })
        }
        ExecutionPortMessage::ModelChunkMessage(message) => {
            validate_model_chunk(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Model,
                stream: model_stream_key(
                    &message.lease,
                    &message.worker_session_id,
                    &message.session_identity,
                    &message.model_exchange_id.0,
                ),
            })
        }
        ExecutionPortMessage::ModelAckMessage(message) => {
            validate_model_ack(message)?;
            Ok(TypedReplayStream {
                kind: ReplayStreamKind::Model,
                stream: model_stream_key(
                    &message.lease,
                    &message.worker_session_id,
                    &message.session_identity,
                    &message.model_exchange_id.0,
                ),
            })
        }
        _ => Err(TypedReplayMappingError::UnsupportedMessage),
    }
}

fn frame_from_runtime_event(
    message: &RuntimeEventMessage,
) -> Result<TypedReplayFrame, TypedReplayMappingError> {
    validate_runtime_event(message)?;
    let stream = runtime_event_stream_key(message);
    let frame = canonical_runtime_frame(message)?;
    Ok(TypedReplayFrame {
        kind: ReplayStreamKind::Runtime,
        stream,
        frame,
    })
}

fn frame_from_artifact_chunk(
    message: &ArtifactChunkMessage,
) -> Result<TypedReplayFrame, TypedReplayMappingError> {
    validate_artifact_chunk(message)?;
    let stream = artifact_stream_key(
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        &message.artifact_id.0,
    );
    let frame = canonical_frame(&message.message_id.0, message.sequence.0, message)?;
    Ok(TypedReplayFrame {
        kind: ReplayStreamKind::Artifact,
        stream,
        frame,
    })
}

fn frame_from_model_chunk(
    message: &ModelChunkMessage,
) -> Result<TypedReplayFrame, TypedReplayMappingError> {
    validate_model_chunk(message)?;
    let stream = model_stream_key(
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        &message.model_exchange_id.0,
    );
    let frame = canonical_frame(&message.message_id.0, message.sequence.0, message)?;
    Ok(TypedReplayFrame {
        kind: ReplayStreamKind::Model,
        stream,
        frame,
    })
}

fn acknowledgement_from_runtime(
    message: &RuntimeAckMessage,
) -> Result<TypedReplayAcknowledgement, TypedReplayMappingError> {
    validate_runtime_ack(message)?;
    Ok(TypedReplayAcknowledgement {
        kind: ReplayStreamKind::Runtime,
        stream: runtime_ack_stream_key(message),
        acknowledgement: generic_acknowledgement(
            &message.status,
            message.ack_sequence.0,
            message
                .replay_from_sequence
                .as_ref()
                .map(|sequence| sequence.0),
        )?,
    })
}

fn acknowledgement_from_artifact(
    message: &ArtifactAckMessage,
) -> Result<TypedReplayAcknowledgement, TypedReplayMappingError> {
    validate_artifact_ack(message)?;
    Ok(TypedReplayAcknowledgement {
        kind: ReplayStreamKind::Artifact,
        stream: artifact_stream_key(
            &message.lease,
            &message.worker_session_id,
            &message.session_identity,
            &message.artifact_id.0,
        ),
        acknowledgement: generic_acknowledgement(
            &message.status,
            message.ack_sequence.0,
            message
                .replay_from_sequence
                .as_ref()
                .map(|sequence| sequence.0),
        )?,
    })
}

fn acknowledgement_from_model(
    message: &ModelAckMessage,
) -> Result<TypedReplayAcknowledgement, TypedReplayMappingError> {
    validate_model_ack(message)?;
    Ok(TypedReplayAcknowledgement {
        kind: ReplayStreamKind::Model,
        stream: model_stream_key(
            &message.lease,
            &message.worker_session_id,
            &message.session_identity,
            &message.model_exchange_id.0,
        ),
        acknowledgement: generic_acknowledgement(
            &message.status,
            message.ack_sequence.0,
            message
                .replay_from_sequence
                .as_ref()
                .map(|sequence| sequence.0),
        )?,
    })
}

fn generic_acknowledgement(
    status: &LeaseWriteStatus,
    ack_sequence: i64,
    replay_from_sequence: Option<i64>,
) -> Result<ReplayAcknowledgement, TypedReplayMappingError> {
    let ack_sequence = ack_sequence_value(ack_sequence)?;
    let status = acknowledgement_status(status);
    let replay_from_sequence = if status == ReplayAcknowledgementStatus::Gap {
        let replay_from_sequence = replay_from_sequence
            .ok_or(TypedReplayMappingError::GapReplayMissing)
            .and_then(sequence_value)?;
        if replay_from_sequence != ack_sequence.saturating_add(1) {
            return Err(TypedReplayMappingError::GapReplayMismatch);
        }
        Some(replay_from_sequence)
    } else {
        if replay_from_sequence.is_some() {
            return Err(TypedReplayMappingError::UnexpectedReplayStart);
        }
        None
    };
    Ok(ReplayAcknowledgement {
        ack_sequence,
        status,
        replay_from_sequence,
    })
}

fn acknowledgement_status(status: &LeaseWriteStatus) -> ReplayAcknowledgementStatus {
    match status {
        LeaseWriteStatus::Accepted => ReplayAcknowledgementStatus::Accepted,
        LeaseWriteStatus::Duplicate => ReplayAcknowledgementStatus::Duplicate,
        LeaseWriteStatus::Gap => ReplayAcknowledgementStatus::Gap,
        LeaseWriteStatus::RejectedConflict => ReplayAcknowledgementStatus::RejectedConflict,
        LeaseWriteStatus::RejectedExpiredLease => ReplayAcknowledgementStatus::RejectedExpiredLease,
        LeaseWriteStatus::RejectedStaleFencingToken => {
            ReplayAcknowledgementStatus::RejectedStaleFencingToken
        }
        LeaseWriteStatus::RejectedWorkerInstance => {
            ReplayAcknowledgementStatus::RejectedWorkerInstance
        }
    }
}

fn canonical_frame<T: Serialize>(
    event_id: &str,
    sequence: i64,
    message: &T,
) -> Result<ReplayFrame, TypedReplayMappingError> {
    if event_id.is_empty() {
        return Err(TypedReplayMappingError::EmptyEventId);
    }
    let sequence = frame_sequence(sequence)?;
    let frame = serde_json::to_vec(message).map_err(|_| TypedReplayMappingError::Serialization)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&frame));
    Ok(ReplayFrame::new(event_id, sequence, digest, frame))
}

fn canonical_runtime_frame(
    message: &RuntimeEventMessage,
) -> Result<ReplayFrame, TypedReplayMappingError> {
    let event_id = &message.event.event_id.0;
    if event_id.is_empty() {
        return Err(TypedReplayMappingError::EmptyEventId);
    }
    let sequence = frame_sequence(message.event.sequence.0)?;
    let frame = serde_json::to_vec(message).map_err(|_| TypedReplayMappingError::Serialization)?;
    let event =
        serde_json::to_vec(&message.event).map_err(|_| TypedReplayMappingError::Serialization)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&event));
    Ok(ReplayFrame::new(event_id, sequence, digest, frame))
}

fn frame_sequence(sequence: i64) -> Result<ReplaySequence, TypedReplayMappingError> {
    let sequence = u64::try_from(sequence).map_err(|_| TypedReplayMappingError::InvalidSequence)?;
    if sequence == 0 {
        return Err(TypedReplayMappingError::InvalidSequence);
    }
    if sequence > MAX_SAFE_INTEGER {
        return Err(TypedReplayMappingError::SequenceOutOfRange);
    }
    Ok(sequence)
}

fn sequence_value(sequence: i64) -> Result<ReplaySequence, TypedReplayMappingError> {
    let sequence =
        u64::try_from(sequence).map_err(|_| TypedReplayMappingError::SequenceOutOfRange)?;
    if sequence > MAX_SAFE_INTEGER {
        return Err(TypedReplayMappingError::SequenceOutOfRange);
    }
    Ok(sequence)
}

fn ack_sequence_value(sequence: i64) -> Result<ReplaySequence, TypedReplayMappingError> {
    if sequence < 0 {
        return Err(TypedReplayMappingError::NegativeAcknowledgement);
    }
    sequence_value(sequence)
}

fn validate_runtime_event(message: &RuntimeEventMessage) -> Result<(), TypedReplayMappingError> {
    if message.kind != RuntimeEventMessageKind::RuntimeEvent {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        false,
    )?;
    if message.codex_thread_id != message.session_identity.codex_thread_id {
        return Err(TypedReplayMappingError::CodexThreadMismatch);
    }
    if message.message_id.0.is_empty() {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    frame_sequence(message.event.sequence.0)?;
    if message.event.event_id.0.is_empty() {
        return Err(TypedReplayMappingError::EmptyEventId);
    }
    Ok(())
}

fn validate_runtime_ack(message: &RuntimeAckMessage) -> Result<(), TypedReplayMappingError> {
    if message.kind != RuntimeAckMessageKind::RuntimeAck {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        false,
    )?;
    if message.message_id.0.is_empty() {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    ack_sequence_value(message.ack_sequence.0)?;
    validate_ack_hint(
        &message.status,
        message.ack_sequence.0,
        message
            .replay_from_sequence
            .as_ref()
            .map(|sequence| sequence.0),
    )
}

fn validate_runtime_replay_request(
    message: &RuntimeReplayRequestMessage,
) -> Result<(), TypedReplayMappingError> {
    if message.kind != RuntimeReplayRequestMessageKind::RuntimeReplayRequest {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        false,
    )?;
    if message.message_id.0.is_empty() || message.request_id.0.is_empty() {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    ack_sequence_value(message.after_sequence.0).map(|_| ())
}

fn validate_artifact_open(message: &ArtifactOpenMessage) -> Result<(), TypedReplayMappingError> {
    if message.kind != ArtifactOpenMessageKind::ArtifactOpen {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        true,
    )?;
    if message.message_id.0.is_empty()
        || message.request_id.0.is_empty()
        || message.artifact.artifact_id.0.is_empty()
    {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    Ok(())
}

fn validate_artifact_chunk(message: &ArtifactChunkMessage) -> Result<(), TypedReplayMappingError> {
    if message.kind != ArtifactChunkMessageKind::ArtifactChunk {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        true,
    )?;
    if message.message_id.0.is_empty() || message.artifact_id.0.is_empty() {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    frame_sequence(message.sequence.0)?;
    Ok(())
}

fn validate_artifact_ack(message: &ArtifactAckMessage) -> Result<(), TypedReplayMappingError> {
    if message.kind != ArtifactAckMessageKind::ArtifactAck {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        true,
    )?;
    if message.message_id.0.is_empty() || message.artifact_id.0.is_empty() {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    validate_ack_hint(
        &message.status,
        message.ack_sequence.0,
        message
            .replay_from_sequence
            .as_ref()
            .map(|sequence| sequence.0),
    )
}

fn validate_model_open(message: &ModelOpenMessage) -> Result<(), TypedReplayMappingError> {
    if message.kind != ModelOpenMessageKind::ModelOpen {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        false,
    )?;
    if message.message_id.0.is_empty()
        || message.request_id.0.is_empty()
        || message.model_exchange_id.0.is_empty()
    {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    Ok(())
}

fn validate_model_chunk(message: &ModelChunkMessage) -> Result<(), TypedReplayMappingError> {
    if message.kind != ModelChunkMessageKind::ModelChunk {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        false,
    )?;
    if message.message_id.0.is_empty() || message.model_exchange_id.0.is_empty() {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    frame_sequence(message.sequence.0)?;
    Ok(())
}

fn validate_model_ack(message: &ModelAckMessage) -> Result<(), TypedReplayMappingError> {
    if message.kind != ModelAckMessageKind::ModelAck {
        return Err(TypedReplayMappingError::WrongKind);
    }
    validate_common(
        &message.schema_version,
        &message.lease,
        &message.worker_session_id,
        &message.session_identity,
        false,
    )?;
    if message.message_id.0.is_empty() || message.model_exchange_id.0.is_empty() {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    validate_ack_hint(
        &message.status,
        message.ack_sequence.0,
        message
            .replay_from_sequence
            .as_ref()
            .map(|sequence| sequence.0),
    )
}

fn validate_ack_hint(
    status: &LeaseWriteStatus,
    ack_sequence: i64,
    replay_from_sequence: Option<i64>,
) -> Result<(), TypedReplayMappingError> {
    ack_sequence_value(ack_sequence)?;
    match status {
        LeaseWriteStatus::Gap => {
            let replay_from_sequence = replay_from_sequence
                .ok_or(TypedReplayMappingError::GapReplayMissing)
                .and_then(sequence_value)?;
            let expected = ack_sequence_value(ack_sequence)?
                .checked_add(1)
                .ok_or(TypedReplayMappingError::SequenceOutOfRange)?;
            if replay_from_sequence != expected {
                return Err(TypedReplayMappingError::GapReplayMismatch);
            }
        }
        _ if replay_from_sequence.is_some() => {
            return Err(TypedReplayMappingError::UnexpectedReplayStart);
        }
        _ => {}
    }
    Ok(())
}

fn validate_common(
    schema_version: &SchemaVersion,
    lease: &ExecutionLeaseStamp,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    session_identity: &SessionIdentity,
    stage_run_required: bool,
) -> Result<(), TypedReplayMappingError> {
    if *schema_version != SchemaVersion::WinwincodeV1 {
        return Err(TypedReplayMappingError::UnsupportedSchema);
    }
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
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    if stage_run_required && session_identity.stage_run_id.is_none() {
        return Err(TypedReplayMappingError::EmptyIdentity);
    }
    if session_identity.worker_session_id != *worker_session_id {
        return Err(TypedReplayMappingError::WorkerSessionMismatch);
    }
    if lease.attempt <= 0 {
        return Err(TypedReplayMappingError::InvalidAttempt);
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
        return Err(TypedReplayMappingError::InvalidFencingToken);
    }
    Ok(())
}

fn artifact_stream_key(
    lease: &ExecutionLeaseStamp,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    session_identity: &SessionIdentity,
    artifact_id: &str,
) -> ReplayStreamKey {
    scoped_stream_key(
        "artifact-worker-replay:v1",
        lease_components(lease, worker_session_id, session_identity, artifact_id),
    )
}

fn model_stream_key(
    lease: &ExecutionLeaseStamp,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    session_identity: &SessionIdentity,
    model_exchange_id: &str,
) -> ReplayStreamKey {
    scoped_stream_key(
        "model-worker-replay:v1",
        lease_components(
            lease,
            worker_session_id,
            session_identity,
            model_exchange_id,
        ),
    )
}

fn lease_components(
    lease: &ExecutionLeaseStamp,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    session_identity: &SessionIdentity,
    resource_id: &str,
) -> Vec<String> {
    vec![
        lease.job_id.0.clone(),
        lease.lease_id.0.clone(),
        lease.worker_id.0.clone(),
        lease.worker_instance_id.0.clone(),
        lease.attempt.to_string(),
        lease.fencing_token.0.clone(),
        worker_session_id.0.clone(),
        session_identity.product_session_id.0.clone(),
        session_identity
            .stage_run_id
            .as_ref()
            .map_or_else(String::new, |stage_run_id| stage_run_id.0.clone()),
        session_identity.codex_thread_id.0.clone(),
        resource_id.to_owned(),
    ]
}

fn scoped_stream_key(prefix: &str, components: Vec<String>) -> ReplayStreamKey {
    let mut value = prefix.to_owned();
    for component in components {
        value.push('/');
        value.push_str(&component.len().to_string());
        value.push(':');
        value.push_str(&component);
    }
    ReplayStreamKey::new(value)
}
