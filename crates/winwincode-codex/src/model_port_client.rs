// SPDX-License-Identifier: Apache-2.0

//! Reliable Worker client for the canonical `ExecutionPort` model stream.
//!
//! The client sends generated `model.open`/`model.ack` messages, accepts only
//! generated `model.chunk` messages for the exact active lease and session,
//! and stores only sequence fingerprints and terminal facts in the cursor.
//! The production bridge keeps an independently keyed, private response-frame
//! ledger for the ProviderFinal/CoreCommitted hand-off; this client never
//! exposes that payload to its cursor implementation. Request payloads are
//! borrowed for the open send and are never retained with cursor state.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ExecutionAckSequence, ExecutionMessageId, ExecutionSequence, Instant, ModelExchangeId,
    RequestId, SchemaVersion, SessionIdentity, Sha256Digest, WorkerSessionId,
};
use winwincode_execution_port::generated::{
    EncodedPayload, ExecutionLeaseStamp, ExecutionPortError, ExecutionPortErrorCode,
    ExecutionPortMessage, LeaseWriteStatus, ModelAckMessage, ModelAckMessageKind,
    ModelChunkMessage, ModelGatewayRoute, ModelOpenMessage, ModelOpenMessageKind,
};
use winwincode_execution_port::replay::ReplayStreamKey;
use winwincode_execution_port::typed_replay::{
    ReplayStreamKind, frame_from_message, stream_key_from_message,
};

use crate::WorkerExecutionPort;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Exact active authority for one model exchange.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelLeaseAuthority {
    pub lease: ExecutionLeaseStamp,
    pub worker_session_id: WorkerSessionId,
    pub session_identity: SessionIdentity,
}

/// Caller-owned current-lease check. Implementations read the same authority
/// that governs the Worker Job and never derive authority from a model frame.
pub trait ModelLeaseAuthoritySource: Send + Sync {
    /// Verifies that the exact lease, attempt, fence, Worker process, and
    /// session remain current at `now`.
    ///
    /// # Errors
    ///
    /// Returns a stable rejection without exposing scheduler internals.
    fn validate_current(
        &self,
        authority: &ModelLeaseAuthority,
        now: &Instant,
    ) -> Result<(), ModelAuthorityRejection>;
}

/// Stable current-authority rejection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelAuthorityRejection {
    ExpiredLease,
    StaleLease,
    Unavailable,
}

/// Transport metadata allocated for one Worker-originated model message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelMessageMetadata {
    pub message_id: ExecutionMessageId,
    pub sent_at: Instant,
}

/// One provider-neutral model request. The request payload is consumed by
/// `open` and is not copied into exchange or cursor state.
#[derive(Clone, PartialEq)]
pub struct OpenModelExchangeCommand {
    pub metadata: ModelMessageMetadata,
    pub authority: ModelLeaseAuthority,
    pub model_exchange_id: ModelExchangeId,
    pub request_id: RequestId,
    pub route: ModelGatewayRoute,
    pub request: EncodedPayload,
}

/// Result of opening one exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelOpenOutcome {
    Opened,
    Duplicate,
}

/// Secret-safe fingerprint retained for one delivered provider frame.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelChunkFingerprint {
    pub sequence: u64,
    pub message_id: ExecutionMessageId,
    pub digest: Sha256Digest,
    pub is_final: bool,
    pub has_error: bool,
}

/// Explicit terminal reason for an exchange.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTerminationReason {
    Completed,
    ProviderError,
    Cancelled,
    MessageConflict,
    InterruptedNotResumable,
    StaleAuthority,
}

/// Durable, secret-safe model cursor. It contains no payload, response text,
/// provider Credential, or Credential reference.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCursorSnapshot {
    pub confirmed_sequence: u64,
    pub frames: Vec<ModelChunkFingerprint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation: Option<ModelCancellationFingerprint>,
    pub termination: Option<ModelTerminationReason>,
}

/// Durable identity of the exact Worker cancellation written to the cursor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelCancellationFingerprint {
    pub message_id: ExecutionMessageId,
    pub confirmed_sequence: u64,
    pub digest: Sha256Digest,
    pub phase: ModelCancellationPhase,
}

/// Durable two-phase cancellation state. `Intent` is written before the
/// idempotent sink interruption; `Applied` is the terminal cursor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCancellationPhase {
    Intent,
    Applied,
}

impl ModelCursorSnapshot {
    /// Validates one adapter-loaded cursor before it controls delivery.
    ///
    /// # Errors
    ///
    /// Rejects non-contiguous, changed, oversized, or inconsistent state.
    pub fn validate(&self) -> Result<(), ModelCursorStateError> {
        if self.confirmed_sequence > MAX_SAFE_INTEGER
            || self.frames.len() as u64 != self.confirmed_sequence
        {
            return Err(ModelCursorStateError::CursorMismatch);
        }
        for (index, frame) in self.frames.iter().enumerate() {
            let expected = u64::try_from(index)
                .ok()
                .and_then(|value| value.checked_add(1))
                .ok_or(ModelCursorStateError::SequenceOverflow)?;
            if frame.sequence != expected {
                return Err(ModelCursorStateError::NonContiguous {
                    expected,
                    found: frame.sequence,
                });
            }
            if frame.message_id.0.is_empty() || !canonical_digest(&frame.digest) {
                return Err(ModelCursorStateError::InvalidFingerprint);
            }
            if frame.is_final && frame.sequence != self.confirmed_sequence {
                return Err(ModelCursorStateError::EarlyFinalFrame);
            }
        }
        if self.termination != Some(ModelTerminationReason::Cancelled)
            && self.termination.is_some()
            && self.cancellation.is_some()
        {
            return Err(ModelCursorStateError::TerminalMismatch);
        }
        match self.termination {
            Some(ModelTerminationReason::Completed) => {
                if !self
                    .frames
                    .last()
                    .is_some_and(|frame| frame.is_final && !frame.has_error)
                {
                    return Err(ModelCursorStateError::TerminalMismatch);
                }
            }
            Some(ModelTerminationReason::ProviderError) => {
                if !self
                    .frames
                    .last()
                    .is_some_and(|frame| frame.is_final && frame.has_error)
                {
                    return Err(ModelCursorStateError::TerminalMismatch);
                }
            }
            Some(ModelTerminationReason::Cancelled) => {
                let Some(cancellation) = self.cancellation.as_ref() else {
                    return Err(ModelCursorStateError::TerminalMismatch);
                };
                if cancellation.phase != ModelCancellationPhase::Applied
                    || !valid_cancellation(cancellation, self.confirmed_sequence)
                {
                    return Err(ModelCursorStateError::InvalidFingerprint);
                }
            }
            None => {
                if self.frames.last().is_some_and(|frame| frame.is_final) {
                    return Err(ModelCursorStateError::TerminalMismatch);
                }
                if self.cancellation.as_ref().is_some_and(|cancellation| {
                    cancellation.phase != ModelCancellationPhase::Intent
                        || !valid_cancellation(cancellation, self.confirmed_sequence)
                }) {
                    return Err(ModelCursorStateError::InvalidFingerprint);
                }
            }
            Some(
                ModelTerminationReason::MessageConflict
                | ModelTerminationReason::InterruptedNotResumable
                | ModelTerminationReason::StaleAuthority,
            ) => {
                if self.cancellation.is_some() {
                    return Err(ModelCursorStateError::TerminalMismatch);
                }
            }
        }
        Ok(())
    }

    fn fingerprint(&self, sequence: u64) -> Option<&ModelChunkFingerprint> {
        let index = usize::try_from(sequence.checked_sub(1)?).ok()?;
        self.frames.get(index)
    }
}

/// Invalid durable model cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelCursorStateError {
    CursorMismatch,
    NonContiguous { expected: u64, found: u64 },
    InvalidFingerprint,
    EarlyFinalFrame,
    TerminalMismatch,
    SequenceOverflow,
}

/// Durable cursor seam. Implementations must compare `expected_sequence` and
/// update a frame fingerprint and its terminal fact atomically.
pub trait ModelCursorStore {
    type Error;

    /// Loads the secret-safe cursor for one canonical stream.
    ///
    /// # Errors
    ///
    /// Returns the adapter's read failure.
    fn load(
        &mut self,
        stream: &ReplayStreamKey,
    ) -> Result<Option<ModelCursorSnapshot>, Self::Error>;

    /// Atomically advances one contiguous delivery.
    ///
    /// # Errors
    ///
    /// Returns the adapter's write or optimistic-cursor failure.
    fn record_delivery(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelChunkFingerprint,
        termination: Option<ModelTerminationReason>,
    ) -> Result<(), Self::Error>;

    /// Atomically records a terminal interruption at the current cursor.
    ///
    /// # Errors
    ///
    /// Returns the adapter's write or optimistic-cursor failure.
    fn terminate(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        reason: ModelTerminationReason,
    ) -> Result<(), Self::Error>;

    /// Atomically records the exact cancellation intent before interrupting
    /// the embedded stream.
    ///
    /// # Errors
    ///
    /// Returns the adapter's write or optimistic-cursor failure.
    fn record_cancellation_intent(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelCancellationFingerprint,
    ) -> Result<(), Self::Error>;

    /// Atomically advances an exact cancellation intent to its terminal
    /// applied state.
    ///
    /// # Errors
    ///
    /// Returns the adapter's write or optimistic-cursor failure.
    fn complete_cancellation(
        &mut self,
        stream: &ReplayStreamKey,
        expected_sequence: u64,
        fingerprint: &ModelCancellationFingerprint,
    ) -> Result<(), Self::Error>;
}

/// Borrowed provider chunk handed to the embedded Codex model stream.
/// Payload and error fields are never copied into the cursor store.
pub struct ModelChunkDelivery<'chunk> {
    pub model_exchange_id: &'chunk ModelExchangeId,
    pub request_id: &'chunk RequestId,
    pub sequence: u64,
    pub payload: Option<&'chunk EncodedPayload>,
    pub is_final: bool,
    pub error: Option<&'chunk ExecutionPortError>,
}

/// Idempotency result from the Codex model-stream sink.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelSinkDeliveryStatus {
    Applied,
    Duplicate,
}

/// Embedded Codex model-stream delivery seam. Implementations use
/// `(model_exchange_id, sequence)` as their idempotency identity.
pub trait ModelChunkSink {
    type Error;

    /// Delivers one contiguous frame to the one embedded Codex stream.
    ///
    /// # Errors
    ///
    /// Returns an adapter failure before the Worker advances its cursor.
    fn deliver(
        &mut self,
        delivery: ModelChunkDelivery<'_>,
    ) -> impl Future<Output = Result<ModelSinkDeliveryStatus, Self::Error>>;

    /// Interrupts one embedded Codex stream using the exchange identity as an
    /// idempotency key.
    ///
    /// # Errors
    ///
    /// Returns an adapter failure before the Worker records termination.
    fn terminate(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        reason: ModelTerminationReason,
    ) -> impl Future<Output = Result<(), Self::Error>>;

    /// Releases the local stream buffers and handles at most once.
    ///
    /// # Errors
    ///
    /// Returns an adapter failure while the terminal cursor remains retryable.
    fn release(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> impl Future<Output = Result<(), Self::Error>>;
}

/// Exact Worker cancellation receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelCancellationReceipt {
    pub confirmed_sequence: u64,
    pub replayed: bool,
}

/// Result of one inbound chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelChunkDisposition {
    Delivered {
        confirmed_sequence: u64,
        termination: Option<ModelTerminationReason>,
    },
    Duplicate {
        confirmed_sequence: u64,
    },
    Gap {
        confirmed_sequence: u64,
        replay_from_sequence: u64,
    },
}

/// Result of an interrupted transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelDisconnectOutcome {
    ResumeRequested {
        confirmed_sequence: u64,
        replay_from_sequence: u64,
    },
    Terminated(ModelTerminationReason),
    AlreadyTerminal(ModelTerminationReason),
}

/// Stable Worker `ModelPort` client failure code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelPortClientErrorCode {
    InvalidInput,
    UnknownExchange,
    ExchangeConflict,
    StaleAuthority,
    ExpiredLease,
    CanonicalMapping,
    CursorStore,
    CursorCorrupt,
    CodexSink,
    ExecutionPort,
    AlreadyTerminal,
}

/// Secret-free Worker `ModelPort` failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelPortClientError {
    code: ModelPortClientErrorCode,
    reason: &'static str,
}

impl ModelPortClientError {
    #[must_use]
    pub const fn code(&self) -> ModelPortClientErrorCode {
        self.code
    }
}

impl fmt::Display for ModelPortClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.reason)
    }
}

impl std::error::Error for ModelPortClientError {}

#[derive(Clone)]
struct ActiveModelExchange {
    authority: ModelLeaseAuthority,
    model_exchange_id: ModelExchangeId,
    request_id: RequestId,
    stream: ReplayStreamKey,
    open_digest: Sha256Digest,
    sink_termination: Option<ModelTerminationReason>,
    sink_released: bool,
    replay_next_sequence: Option<u64>,
    replay_through_sequence: u64,
}

/// Reliable model-stream client on the Worker's canonical `ExecutionPort`.
pub struct WorkerModelPortClient<Port, Store, Authority, Sink> {
    port: Port,
    store: Store,
    authority: Authority,
    sink: Sink,
    active: HashMap<String, ActiveModelExchange>,
}

impl<Port, Store, Authority, Sink> WorkerModelPortClient<Port, Store, Authority, Sink>
where
    Port: WorkerExecutionPort,
    Store: ModelCursorStore,
    Authority: ModelLeaseAuthoritySource,
    Sink: ModelChunkSink,
{
    #[must_use]
    pub fn new(port: Port, store: Store, authority: Authority, sink: Sink) -> Self {
        Self {
            port,
            store,
            authority,
            sink,
            active: HashMap::new(),
        }
    }

    /// Opens one exact provider-neutral exchange.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, conflicting exchange reuse, invalid canonical
    /// messages, and transport failures. No request payload is retained.
    pub async fn open(
        &mut self,
        command: OpenModelExchangeCommand,
    ) -> Result<ModelOpenOutcome, ModelPortClientError> {
        require_authority_shape(&command.authority)?;
        self.require_current(&command.authority, &command.metadata.sent_at)?;
        let message = ModelOpenMessage {
            kind: ModelOpenMessageKind::ModelOpen,
            schema_version: SchemaVersion::WinwincodeV1,
            message_id: command.metadata.message_id,
            sent_at: command.metadata.sent_at,
            request_id: command.request_id.clone(),
            lease: command.authority.lease.clone(),
            worker_session_id: command.authority.worker_session_id.clone(),
            model_exchange_id: command.model_exchange_id.clone(),
            route: command.route,
            request: command.request,
            session_identity: command.authority.session_identity.clone(),
        };
        let stream = canonical_stream(&ExecutionPortMessage::ModelOpenMessage(message.clone()))?;
        let open_digest = open_digest(&message)?;
        if let Some(active) = self.active.get(&message.model_exchange_id.0) {
            if active.open_digest == open_digest
                && active.authority == command.authority
                && active.request_id == message.request_id
                && active.stream == stream
            {
                return Ok(ModelOpenOutcome::Duplicate);
            }
            return Err(client_error(
                ModelPortClientErrorCode::ExchangeConflict,
                "model exchange identity was reused with different input",
            ));
        }
        let snapshot = self.load_cursor(&stream)?;
        self.port
            .send(ExecutionPortMessage::ModelOpenMessage(message.clone()))
            .await
            .map_err(|_| execution_port_error())?;
        self.active.insert(
            message.model_exchange_id.0.clone(),
            ActiveModelExchange {
                authority: command.authority,
                model_exchange_id: message.model_exchange_id,
                request_id: message.request_id,
                stream,
                open_digest,
                sink_termination: None,
                sink_released: false,
                replay_next_sequence: (snapshot.confirmed_sequence > 0).then_some(1),
                replay_through_sequence: snapshot.confirmed_sequence,
            },
        );
        Ok(ModelOpenOutcome::Opened)
    }

    /// Delivers one canonical chunk at most once and sends the corresponding
    /// contiguous acknowledgement or gap replay request.
    ///
    /// # Errors
    ///
    /// Rejects stale/cross-stream authority before Codex or cursor mutation.
    pub async fn accept_chunk(
        &mut self,
        chunk: &ModelChunkMessage,
        acknowledgement: ModelMessageMetadata,
    ) -> Result<ModelChunkDisposition, ModelPortClientError> {
        let active = self.active_exchange(&chunk.model_exchange_id)?.clone();
        self.require_current(&active.authority, &acknowledgement.sent_at)?;
        require_exact_chunk(&active, chunk)?;
        let mapped = frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
            .map_err(|_| canonical_mapping_error())?;
        if mapped.kind != ReplayStreamKind::Model || mapped.stream != active.stream {
            return Err(stale_authority_error());
        }
        let sequence = mapped.frame.sequence;
        let fingerprint = ModelChunkFingerprint {
            sequence,
            message_id: chunk.message_id.clone(),
            digest: Sha256Digest(mapped.frame.digest),
            is_final: chunk.is_final,
            has_error: chunk.error.is_some(),
        };
        if chunk.error.is_some() && !chunk.is_final {
            return Err(invalid("a model stream error must be terminal"));
        }
        let snapshot = self.load_cursor(&active.stream)?;
        if snapshot.cancellation.is_some() {
            return Err(cancellation_pending_error());
        }
        if sequence <= snapshot.confirmed_sequence {
            return self
                .handle_duplicate(&active, &snapshot, &fingerprint, chunk, acknowledgement)
                .await;
        }
        let expected = snapshot
            .confirmed_sequence
            .checked_add(1)
            .ok_or_else(cursor_corrupt_error)?;
        if sequence > expected {
            self.send_ack(
                &active,
                acknowledgement,
                LeaseWriteStatus::Gap,
                snapshot.confirmed_sequence,
                Some(expected),
                None,
            )
            .await?;
            return Ok(ModelChunkDisposition::Gap {
                confirmed_sequence: snapshot.confirmed_sequence,
                replay_from_sequence: expected,
            });
        }
        if let Some(reason) = snapshot.termination {
            return Err(client_error(
                ModelPortClientErrorCode::AlreadyTerminal,
                terminal_reason(reason),
            ));
        }
        self.sink
            .deliver(ModelChunkDelivery {
                model_exchange_id: &chunk.model_exchange_id,
                request_id: &active.request_id,
                sequence,
                payload: chunk.payload.as_ref(),
                is_final: chunk.is_final,
                error: chunk.error.as_ref(),
            })
            .await
            .map_err(|_| {
                client_error(
                    ModelPortClientErrorCode::CodexSink,
                    "embedded Codex model stream rejected a chunk",
                )
            })?;
        let termination = chunk.is_final.then_some(if chunk.error.is_some() {
            ModelTerminationReason::ProviderError
        } else {
            ModelTerminationReason::Completed
        });
        self.store
            .record_delivery(
                &active.stream,
                snapshot.confirmed_sequence,
                &fingerprint,
                termination,
            )
            .map_err(|_| cursor_store_error())?;
        if termination.is_some() {
            self.release_sink_once(&active).await?;
        }
        self.send_ack(
            &active,
            acknowledgement,
            LeaseWriteStatus::Accepted,
            sequence,
            None,
            None,
        )
        .await?;
        Ok(ModelChunkDisposition::Delivered {
            confirmed_sequence: sequence,
            termination,
        })
    }

    /// Acknowledges an exact terminal frame after the process-local exchange
    /// handle has been released.
    ///
    /// The durable cursor is the source of truth for this path.  It is used
    /// when a Provider retries a final frame during the small window after a
    /// Worker restart, before Core has issued the corresponding replay open.
    /// No sink is touched and no new Core work is created.
    pub(crate) async fn accept_terminal_duplicate(
        &mut self,
        authority: ModelLeaseAuthority,
        chunk: &ModelChunkMessage,
        acknowledgement: ModelMessageMetadata,
    ) -> Result<ModelChunkDisposition, ModelPortClientError> {
        require_authority_shape(&authority)?;
        self.require_current(&authority, &acknowledgement.sent_at)?;
        if chunk.lease != authority.lease
            || chunk.worker_session_id != authority.worker_session_id
            || chunk.session_identity != authority.session_identity
        {
            return Err(stale_authority_error());
        }
        let mapped = frame_from_message(&ExecutionPortMessage::ModelChunkMessage(chunk.clone()))
            .map_err(|_| canonical_mapping_error())?;
        if mapped.kind != ReplayStreamKind::Model {
            return Err(canonical_mapping_error());
        }
        let sequence = u64::try_from(chunk.sequence.0).map_err(|_| cursor_corrupt_error())?;
        if sequence == 0 {
            return Err(invalid("model chunk sequence must be positive"));
        }
        let snapshot = self.load_cursor(&mapped.stream)?;
        if !matches!(
            snapshot.termination,
            Some(ModelTerminationReason::Completed | ModelTerminationReason::ProviderError)
        ) {
            return Err(client_error(
                ModelPortClientErrorCode::AlreadyTerminal,
                "model exchange does not have a provider terminal cursor",
            ));
        }
        let fingerprint = ModelChunkFingerprint {
            sequence,
            message_id: chunk.message_id.clone(),
            digest: Sha256Digest(mapped.frame.digest),
            is_final: chunk.is_final,
            has_error: chunk.error.is_some(),
        };
        let active = ActiveModelExchange {
            authority,
            model_exchange_id: chunk.model_exchange_id.clone(),
            // The request id is not part of a model acknowledgement.  Keep a
            // stable placeholder so this synthetic active exchange remains
            // structurally complete while no Core sink is attached.
            request_id: RequestId("replayed-model-call".to_owned()),
            stream: mapped.stream,
            open_digest: Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(chunk.message_id.0.as_bytes())
            )),
            sink_termination: snapshot.termination,
            sink_released: true,
            replay_next_sequence: None,
            replay_through_sequence: snapshot.confirmed_sequence,
        };
        if snapshot.fingerprint(sequence) != Some(&fingerprint) {
            self.send_ack(
                &active,
                acknowledgement,
                LeaseWriteStatus::RejectedConflict,
                snapshot.confirmed_sequence,
                None,
                Some(ExecutionPortError {
                    code: ExecutionPortErrorCode::MessageConflict,
                    message: "model stream sequence conflicts with confirmed content".into(),
                    retryable: false,
                }),
            )
            .await?;
            return Err(client_error(
                ModelPortClientErrorCode::ExchangeConflict,
                "model stream sequence conflicts with confirmed content",
            ));
        }
        self.send_ack(
            &active,
            acknowledgement,
            LeaseWriteStatus::Duplicate,
            snapshot.confirmed_sequence,
            None,
            None,
        )
        .await?;
        Ok(ModelChunkDisposition::Duplicate {
            confirmed_sequence: snapshot.confirmed_sequence,
        })
    }

    /// Cancels one exact model exchange under its current lease and fencing
    /// authority. Exact retries resend the same canonical acknowledgement
    /// without interrupting the embedded stream twice.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, changed cancellation identity, other terminal
    /// outcomes, cursor failures, sink failures, and transport failures.
    pub async fn cancel_exchange(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        metadata: ModelMessageMetadata,
    ) -> Result<ModelCancellationReceipt, ModelPortClientError> {
        let active = self.active_exchange(model_exchange_id)?.clone();
        self.require_current(&active.authority, &metadata.sent_at)?;
        let snapshot = self.load_cursor(&active.stream)?;
        let fingerprint =
            cancellation_fingerprint(&active, &metadata, snapshot.confirmed_sequence)?;
        if snapshot
            .cancellation
            .as_ref()
            .is_some_and(|existing| !same_cancellation_identity(existing, &fingerprint))
        {
            return Err(client_error(
                ModelPortClientErrorCode::ExchangeConflict,
                "model cancellation identity was reused with different input",
            ));
        }
        if let Some(reason) = snapshot.termination
            && reason != ModelTerminationReason::Cancelled
        {
            return Err(client_error(
                ModelPortClientErrorCode::AlreadyTerminal,
                terminal_reason(reason),
            ));
        }
        let replayed = snapshot.cancellation.is_some();
        if snapshot.cancellation.is_none() {
            self.store
                .record_cancellation_intent(
                    &active.stream,
                    snapshot.confirmed_sequence,
                    &fingerprint,
                )
                .map_err(|_| cursor_store_error())?;
        }
        if snapshot.termination != Some(ModelTerminationReason::Cancelled) {
            self.terminate_sink_once(&active, ModelTerminationReason::Cancelled)
                .await?;
            self.store
                .complete_cancellation(&active.stream, snapshot.confirmed_sequence, &fingerprint)
                .map_err(|_| cursor_store_error())?;
        }
        self.release_sink_once(&active).await?;
        self.send_cancellation_ack(&active, metadata, snapshot.confirmed_sequence)
            .await?;
        Ok(ModelCancellationReceipt {
            confirmed_sequence: snapshot.confirmed_sequence,
            replayed,
        })
    }

    /// Forgets the process-local exchange after a terminal acknowledgement has
    /// been accepted by the Gateway. The durable cursor remains authoritative.
    ///
    /// # Errors
    ///
    /// Rejects an active cursor and propagates sink or cursor failures.
    pub async fn release_terminal(
        &mut self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<ModelTerminationReason, ModelPortClientError> {
        let active = self.active_exchange(model_exchange_id)?.clone();
        let snapshot = self.load_cursor(&active.stream)?;
        let reason = snapshot.termination.ok_or_else(|| {
            client_error(
                ModelPortClientErrorCode::AlreadyTerminal,
                "model exchange is not terminal",
            )
        })?;
        self.release_sink_once(&active).await?;
        self.active.remove(&model_exchange_id.0);
        Ok(reason)
    }

    /// Requests replay after the highest confirmed Codex delivery, or records
    /// an explicit terminal interruption when resume is not permitted.
    ///
    /// # Errors
    ///
    /// Returns cursor or transport failures. A stale authority is durably
    /// terminated without emitting a write under the old lease.
    pub async fn handle_disconnect(
        &mut self,
        model_exchange_id: &ModelExchangeId,
        metadata: ModelMessageMetadata,
        resumable: bool,
    ) -> Result<ModelDisconnectOutcome, ModelPortClientError> {
        let active = self.active_exchange(model_exchange_id)?.clone();
        let snapshot = self.load_cursor(&active.stream)?;
        if snapshot.cancellation.is_some() {
            return Err(cancellation_pending_error());
        }
        if let Some(reason) = snapshot.termination {
            return Ok(ModelDisconnectOutcome::AlreadyTerminal(reason));
        }
        if self
            .require_current(&active.authority, &metadata.sent_at)
            .is_err()
        {
            self.terminate_sink_once(&active, ModelTerminationReason::StaleAuthority)
                .await?;
            self.store
                .terminate(
                    &active.stream,
                    snapshot.confirmed_sequence,
                    ModelTerminationReason::StaleAuthority,
                )
                .map_err(|_| cursor_store_error())?;
            self.release_sink_once(&active).await?;
            return Ok(ModelDisconnectOutcome::Terminated(
                ModelTerminationReason::StaleAuthority,
            ));
        }
        let replay_from = snapshot
            .confirmed_sequence
            .checked_add(1)
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(cursor_corrupt_error)?;
        if resumable {
            self.send_ack(
                &active,
                metadata,
                LeaseWriteStatus::Gap,
                snapshot.confirmed_sequence,
                Some(replay_from),
                None,
            )
            .await?;
            return Ok(ModelDisconnectOutcome::ResumeRequested {
                confirmed_sequence: snapshot.confirmed_sequence,
                replay_from_sequence: replay_from,
            });
        }
        self.terminate_sink_once(&active, ModelTerminationReason::InterruptedNotResumable)
            .await?;
        self.store
            .terminate(
                &active.stream,
                snapshot.confirmed_sequence,
                ModelTerminationReason::InterruptedNotResumable,
            )
            .map_err(|_| cursor_store_error())?;
        self.release_sink_once(&active).await?;
        self.send_ack(
            &active,
            metadata,
            LeaseWriteStatus::RejectedConflict,
            snapshot.confirmed_sequence,
            None,
            Some(ExecutionPortError {
                code: ExecutionPortErrorCode::ModelStreamFailed,
                message: "model stream interruption is not resumable".into(),
                retryable: false,
            }),
        )
        .await?;
        Ok(ModelDisconnectOutcome::Terminated(
            ModelTerminationReason::InterruptedNotResumable,
        ))
    }

    #[must_use]
    pub const fn port(&self) -> &Port {
        &self.port
    }

    #[must_use]
    pub const fn store(&self) -> &Store {
        &self.store
    }

    #[must_use]
    pub const fn sink(&self) -> &Sink {
        &self.sink
    }

    fn active_exchange(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<&ActiveModelExchange, ModelPortClientError> {
        self.active.get(&model_exchange_id.0).ok_or_else(|| {
            client_error(
                ModelPortClientErrorCode::UnknownExchange,
                "model exchange is not open on this Worker",
            )
        })
    }

    fn require_current(
        &self,
        authority: &ModelLeaseAuthority,
        now: &Instant,
    ) -> Result<(), ModelPortClientError> {
        if !canonical_instant(now) || now.0 >= authority.lease.expires_at.0 {
            return Err(client_error(
                ModelPortClientErrorCode::ExpiredLease,
                "model exchange lease has expired",
            ));
        }
        self.authority
            .validate_current(authority, now)
            .map_err(|rejection| match rejection {
                ModelAuthorityRejection::ExpiredLease => client_error(
                    ModelPortClientErrorCode::ExpiredLease,
                    "model exchange lease has expired",
                ),
                ModelAuthorityRejection::StaleLease | ModelAuthorityRejection::Unavailable => {
                    stale_authority_error()
                }
            })
    }

    fn load_cursor(
        &mut self,
        stream: &ReplayStreamKey,
    ) -> Result<ModelCursorSnapshot, ModelPortClientError> {
        let snapshot = self
            .store
            .load(stream)
            .map_err(|_| cursor_store_error())?
            .unwrap_or_default();
        snapshot.validate().map_err(|_| cursor_corrupt_error())?;
        Ok(snapshot)
    }

    async fn handle_duplicate(
        &mut self,
        active: &ActiveModelExchange,
        snapshot: &ModelCursorSnapshot,
        fingerprint: &ModelChunkFingerprint,
        chunk: &ModelChunkMessage,
        metadata: ModelMessageMetadata,
    ) -> Result<ModelChunkDisposition, ModelPortClientError> {
        if snapshot.fingerprint(fingerprint.sequence) != Some(fingerprint) {
            if snapshot.termination.is_none() {
                self.terminate_sink_once(active, ModelTerminationReason::MessageConflict)
                    .await?;
                self.store
                    .terminate(
                        &active.stream,
                        snapshot.confirmed_sequence,
                        ModelTerminationReason::MessageConflict,
                    )
                    .map_err(|_| cursor_store_error())?;
                self.release_sink_once(active).await?;
            }
            self.send_ack(
                active,
                metadata,
                LeaseWriteStatus::RejectedConflict,
                snapshot.confirmed_sequence,
                None,
                Some(ExecutionPortError {
                    code: ExecutionPortErrorCode::MessageConflict,
                    message: "model stream sequence was replayed with different content".into(),
                    retryable: false,
                }),
            )
            .await?;
            return Err(client_error(
                ModelPortClientErrorCode::ExchangeConflict,
                "model stream sequence conflicts with confirmed content",
            ));
        }
        let replay_to_new_sink =
            self.active
                .get(&active.model_exchange_id.0)
                .is_some_and(|current| {
                    current.replay_next_sequence == Some(fingerprint.sequence)
                        && fingerprint.sequence <= current.replay_through_sequence
                });
        if replay_to_new_sink {
            self.sink
                .deliver(ModelChunkDelivery {
                    model_exchange_id: &chunk.model_exchange_id,
                    request_id: &active.request_id,
                    sequence: fingerprint.sequence,
                    payload: chunk.payload.as_ref(),
                    is_final: chunk.is_final,
                    error: chunk.error.as_ref(),
                })
                .await
                .map_err(|_| codex_sink_error())?;
            if let Some(current) = self.active.get_mut(&active.model_exchange_id.0) {
                current.replay_next_sequence = (fingerprint.sequence
                    < current.replay_through_sequence)
                    .then_some(fingerprint.sequence.saturating_add(1));
            }
        }
        if snapshot.termination.is_some() && chunk.is_final {
            self.release_sink_once(active).await?;
        }
        self.send_ack(
            active,
            metadata,
            LeaseWriteStatus::Duplicate,
            snapshot.confirmed_sequence,
            None,
            None,
        )
        .await?;
        Ok(ModelChunkDisposition::Duplicate {
            confirmed_sequence: snapshot.confirmed_sequence,
        })
    }

    async fn terminate_sink_once(
        &mut self,
        active: &ActiveModelExchange,
        reason: ModelTerminationReason,
    ) -> Result<(), ModelPortClientError> {
        if let Some(existing) = self
            .active_exchange(&active.model_exchange_id)?
            .sink_termination
        {
            if existing == reason {
                return Ok(());
            }
            return Err(client_error(
                ModelPortClientErrorCode::AlreadyTerminal,
                "embedded Codex model stream already has another terminal outcome",
            ));
        }
        self.sink
            .terminate(&active.model_exchange_id, reason)
            .await
            .map_err(|_| codex_sink_error())?;
        self.active
            .get_mut(&active.model_exchange_id.0)
            .ok_or_else(|| {
                client_error(
                    ModelPortClientErrorCode::UnknownExchange,
                    "model exchange is not open on this Worker",
                )
            })?
            .sink_termination = Some(reason);
        Ok(())
    }

    async fn release_sink_once(
        &mut self,
        active: &ActiveModelExchange,
    ) -> Result<(), ModelPortClientError> {
        if self
            .active_exchange(&active.model_exchange_id)?
            .sink_released
        {
            return Ok(());
        }
        self.sink
            .release(&active.model_exchange_id)
            .await
            .map_err(|_| codex_sink_error())?;
        self.active
            .get_mut(&active.model_exchange_id.0)
            .ok_or_else(|| {
                client_error(
                    ModelPortClientErrorCode::UnknownExchange,
                    "model exchange is not open on this Worker",
                )
            })?
            .sink_released = true;
        Ok(())
    }

    async fn send_cancellation_ack(
        &mut self,
        active: &ActiveModelExchange,
        metadata: ModelMessageMetadata,
        confirmed_sequence: u64,
    ) -> Result<(), ModelPortClientError> {
        self.send_ack(
            active,
            metadata,
            LeaseWriteStatus::RejectedConflict,
            confirmed_sequence,
            None,
            Some(ExecutionPortError {
                code: ExecutionPortErrorCode::Cancelled,
                message: "model exchange cancelled by Worker".into(),
                retryable: false,
            }),
        )
        .await
    }

    async fn send_ack(
        &mut self,
        active: &ActiveModelExchange,
        metadata: ModelMessageMetadata,
        status: LeaseWriteStatus,
        acknowledged: u64,
        replay_from: Option<u64>,
        error: Option<ExecutionPortError>,
    ) -> Result<(), ModelPortClientError> {
        let ack_sequence = i64::try_from(acknowledged).map_err(|_| cursor_corrupt_error())?;
        let replay_from_sequence = replay_from
            .map(|value| {
                i64::try_from(value)
                    .map(ExecutionSequence)
                    .map_err(|_| cursor_corrupt_error())
            })
            .transpose()?;
        let message = ModelAckMessage {
            ack_sequence: ExecutionAckSequence(ack_sequence),
            error,
            kind: ModelAckMessageKind::ModelAck,
            lease: active.authority.lease.clone(),
            message_id: metadata.message_id,
            model_exchange_id: active.model_exchange_id.clone(),
            replay_from_sequence,
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: metadata.sent_at,
            session_identity: active.authority.session_identity.clone(),
            status,
            worker_session_id: active.authority.worker_session_id.clone(),
        };
        let outgoing = ExecutionPortMessage::ModelAckMessage(message);
        if canonical_stream(&outgoing)? != active.stream {
            return Err(stale_authority_error());
        }
        self.port
            .send(outgoing)
            .await
            .map_err(|_| execution_port_error())
    }
}

fn canonical_stream(
    message: &ExecutionPortMessage,
) -> Result<ReplayStreamKey, ModelPortClientError> {
    let mapped = stream_key_from_message(message).map_err(|_| canonical_mapping_error())?;
    if mapped.kind != ReplayStreamKind::Model {
        return Err(canonical_mapping_error());
    }
    Ok(mapped.stream)
}

fn require_authority_shape(authority: &ModelLeaseAuthority) -> Result<(), ModelPortClientError> {
    let lease = &authority.lease;
    if lease.job_id.0.is_empty()
        || lease.lease_id.0.is_empty()
        || lease.worker_id.0.is_empty()
        || lease.worker_instance_id.0.is_empty()
        || authority.worker_session_id.0.is_empty()
        || authority.session_identity.product_session_id.0.is_empty()
        || authority.session_identity.worker_session_id != authority.worker_session_id
        || authority.session_identity.codex_thread_id.0.is_empty()
        || lease.attempt <= 0
        || lease.attempt > 1_000
        || !canonical_fence(&lease.fencing_token.0)
        || !canonical_instant(&lease.issued_at)
        || !canonical_instant(&lease.expires_at)
        || lease.issued_at.0 >= lease.expires_at.0
    {
        return Err(invalid("model exchange authority is invalid"));
    }
    Ok(())
}

fn require_exact_chunk(
    active: &ActiveModelExchange,
    chunk: &ModelChunkMessage,
) -> Result<(), ModelPortClientError> {
    if chunk.lease != active.authority.lease
        || chunk.worker_session_id != active.authority.worker_session_id
        || chunk.session_identity != active.authority.session_identity
    {
        return Err(stale_authority_error());
    }
    Ok(())
}

fn open_digest(message: &ModelOpenMessage) -> Result<Sha256Digest, ModelPortClientError> {
    let bytes = serde_json::to_vec(&(
        &message.request_id,
        &message.lease,
        &message.worker_session_id,
        &message.model_exchange_id,
        &message.route,
        &message.request,
        &message.session_identity,
    ))
    .map_err(|_| canonical_mapping_error())?;
    Ok(Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))))
}

fn cancellation_fingerprint(
    active: &ActiveModelExchange,
    metadata: &ModelMessageMetadata,
    confirmed_sequence: u64,
) -> Result<ModelCancellationFingerprint, ModelPortClientError> {
    let bytes = serde_json::to_vec(&(
        &active.model_exchange_id,
        &active.authority.lease,
        &active.authority.worker_session_id,
        &active.authority.session_identity,
        &metadata.message_id,
        &metadata.sent_at,
        confirmed_sequence,
        "cancelled",
    ))
    .map_err(|_| canonical_mapping_error())?;
    Ok(ModelCancellationFingerprint {
        message_id: metadata.message_id.clone(),
        confirmed_sequence,
        digest: Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
        phase: ModelCancellationPhase::Intent,
    })
}

fn valid_cancellation(
    cancellation: &ModelCancellationFingerprint,
    confirmed_sequence: u64,
) -> bool {
    !cancellation.message_id.0.is_empty()
        && cancellation.confirmed_sequence == confirmed_sequence
        && canonical_digest(&cancellation.digest)
}

fn same_cancellation_identity(
    left: &ModelCancellationFingerprint,
    right: &ModelCancellationFingerprint,
) -> bool {
    left.message_id == right.message_id
        && left.confirmed_sequence == right.confirmed_sequence
        && left.digest == right.digest
}

fn canonical_fence(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 20
        && !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn canonical_instant(instant: &Instant) -> bool {
    let value = instant.0.as_bytes();
    value.len() == 24
        && value[4] == b'-'
        && value[7] == b'-'
        && value[10] == b'T'
        && value[13] == b':'
        && value[16] == b':'
        && value[19] == b'.'
        && value[23] == b'Z'
        && value.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
}

fn canonical_digest(digest: &Sha256Digest) -> bool {
    digest.0.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

const fn terminal_reason(reason: ModelTerminationReason) -> &'static str {
    match reason {
        ModelTerminationReason::Completed => "model exchange already completed",
        ModelTerminationReason::ProviderError => "model exchange ended with a Provider error",
        ModelTerminationReason::Cancelled => "model exchange was cancelled",
        ModelTerminationReason::MessageConflict => "model exchange ended after a message conflict",
        ModelTerminationReason::InterruptedNotResumable => {
            "model exchange interruption was not resumable"
        }
        ModelTerminationReason::StaleAuthority => {
            "model exchange ended after its authority became stale"
        }
    }
}

const fn invalid(reason: &'static str) -> ModelPortClientError {
    client_error(ModelPortClientErrorCode::InvalidInput, reason)
}

const fn stale_authority_error() -> ModelPortClientError {
    client_error(
        ModelPortClientErrorCode::StaleAuthority,
        "model stream authority is stale or cross-scoped",
    )
}

const fn canonical_mapping_error() -> ModelPortClientError {
    client_error(
        ModelPortClientErrorCode::CanonicalMapping,
        "model message does not match the canonical ExecutionPort stream",
    )
}

const fn cursor_store_error() -> ModelPortClientError {
    client_error(
        ModelPortClientErrorCode::CursorStore,
        "model cursor store is unavailable",
    )
}

const fn cursor_corrupt_error() -> ModelPortClientError {
    client_error(
        ModelPortClientErrorCode::CursorCorrupt,
        "model cursor state is invalid",
    )
}

const fn cancellation_pending_error() -> ModelPortClientError {
    client_error(
        ModelPortClientErrorCode::AlreadyTerminal,
        "model exchange cancellation is pending",
    )
}

const fn codex_sink_error() -> ModelPortClientError {
    client_error(
        ModelPortClientErrorCode::CodexSink,
        "embedded Codex model stream could not release its resources",
    )
}

const fn execution_port_error() -> ModelPortClientError {
    client_error(
        ModelPortClientErrorCode::ExecutionPort,
        "canonical ExecutionPort model message could not be sent",
    )
}

const fn client_error(
    code: ModelPortClientErrorCode,
    reason: &'static str,
) -> ModelPortClientError {
    ModelPortClientError { code, reason }
}
