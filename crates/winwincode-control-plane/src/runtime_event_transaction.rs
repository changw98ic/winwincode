// SPDX-License-Identifier: Apache-2.0

//! Receipt-first ingress for generated Worker `runtime.event` messages.
//!
//! The wire message is the only input at this seam.  Product and Delivery
//! identities are joined from the durable dispatch intent and the current
//! Delivery snapshot; a Worker cannot provide a second scope or session map.
//! Accepted events are kept in a dedicated Control Plane state stream so a
//! runtime append never mutates a Delivery member.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_api::generated::{
    ControlPlaneWebSocketDeliveryGetReloadQuery,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind,
    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue,
    ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent,
    ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventScopeKind,
    ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventTypeValue,
    ControlPlaneWebSocketRuntimeProjectionGetReloadQuery, RepositoryScope,
};
use winwincode_audit::{
    AuditAction, AuditEvent, AuditEventId, AuditExecutionIdentity, AuditExecutionSubjectKind,
    AuditScope, AuditState, AuditSubject,
};
use winwincode_delivery::{
    application::stage::SessionBindingAuthority,
    domain::{Delivery, StageRunStatus},
};
use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence, ExecutionJobId,
    ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId, ProductSessionId,
    Revision, SchemaVersion, SessionIdentity, Sha256Digest, StageRunId, WorkerId, WorkerInstanceId,
    WorkerSessionId,
};
use winwincode_execution_port::generated::{
    DeliveryStageExecutionScopeKind, ExecutionEventRecord, ExecutionJob, ExecutionLeaseStamp,
    ExecutionPortError, ExecutionPortErrorCode, ExecutionScope, LeaseWriteStatus,
    ProductSessionExecutionScopeKind, RuntimeAckMessage, RuntimeAckMessageKind,
    RuntimeEventMessage, RuntimeEventMessageKind,
};
use winwincode_storage::{
    CommitReceipt, DurableOutboxEvent, NewOutboxEvent, PendingAuditEvent, ProductStateStorage,
    ProjectionEventStream, PublicEventSource, ReceiptIdentity, ReceiptScopeKey, StateCommit,
    StateRevisionGuard, StorageError, StorageErrorKind,
};

use crate::delivery_transaction::{EXECUTION_JOB_TOPIC, delivery_stream_id};
use crate::product_session_service::catalog_stream_id;
use crate::session_binding_transaction::{
    execution_message_actor_key, execution_message_request_id, instant_millis, projection_event_id,
    require_id,
};
use crate::{
    OutboxError, execution_audit_event_with_state, repository_scope_from_receipt_key,
    repository_scope_key,
};

const RUNTIME_PHASE: &str = "runtime-event";
const RUNTIME_TOPIC: &str = "runtime.event.accepted.v1";
const RUNTIME_INVALIDATED_TOPIC: &str = "runtime-projection.invalidated.v1";
const RUNTIME_STREAM_PREFIX: &str = "runtime:";
const RUNTIME_STATE_VERSION: u8 = 1;
const RUNTIME_INVALIDATION_NAMESPACE: &[u8] = b"winwincode.runtime-event-runtime-invalidation.v1";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_SUMMARY_LENGTH: usize = 2_000;
const ACK_ID_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Failure after a runtime event's semantic result was not available.
///
/// Rejected Worker input is deliberately returned as a generated
/// [`RuntimeAckMessage`].  This error therefore represents only storage or
/// publication infrastructure failures.
#[derive(Clone, Debug, PartialEq)]
pub enum RuntimeMessageError {
    /// No runtime ledger mutation was committed.
    Storage(StorageError),
    /// The accepted ledger row is durable, but its notification remains in
    /// the outbox for replay.
    PublicationPending {
        ack: Box<RuntimeAckMessage>,
        source: OutboxError,
    },
}

impl RuntimeMessageError {
    /// Returns the acknowledgement that was durably accepted before
    /// publication failed.
    #[must_use]
    pub fn committed_ack(&self) -> Option<&RuntimeAckMessage> {
        match self {
            Self::Storage(_) => None,
            Self::PublicationPending { ack, .. } => Some(ack),
        }
    }
}

impl fmt::Display for RuntimeMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "runtime event transaction failed: {error}"),
            Self::PublicationPending { source, .. } => write!(
                formatter,
                "runtime event accepted, but its notification remains pending: {source}"
            ),
        }
    }
}

impl std::error::Error for RuntimeMessageError {}

impl From<StorageError> for RuntimeMessageError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Executes one generated runtime-event ingress against the durable product
/// state and its independent runtime ledger stream.
#[allow(clippy::too_many_lines)]
pub(crate) fn execute_at(
    storage: &mut dyn ProductStateStorage,
    scope: &RepositoryScope,
    message: &RuntimeEventMessage,
    authority: &SessionBindingAuthority,
    server_time: &Instant,
) -> Result<RuntimeAckMessage, RuntimeMessageError> {
    let scope_key = repository_scope_key(scope)?;
    let phase = RuntimePhase::new(&scope_key, message)?;

    // The receipt is checked before current state so a crash/restart can
    // replay the original accepted acknowledgement without reinterpreting a
    // changed live Delivery snapshot.
    match storage.load_receipt(&phase.receipt_identity, &phase.command_digest) {
        Ok(Some(receipt)) => {
            return replay_ack(&receipt, &phase, message).map_err(RuntimeMessageError::from);
        }
        Ok(None) => {}
        Err(error) if error.kind() == StorageErrorKind::RequestConflict => {
            // A changed body is still a receipt conflict. Ask the storage
            // seam for the existing receipt rather than loading the current
            // aggregate, journal, or runtime facts. Adapters that do not
            // expose this optional helper return a deterministic zero cursor.
            let acknowledged_sequence = prior_receipt_sequence(storage, &phase)?;
            return Ok(rejection_ack(
                message,
                acknowledged_sequence,
                Rejection::Conflict("runtime message identity was reused with different content"),
            ));
        }
        Err(error) => return Err(error.into()),
    }

    if let Err(rejection) = validate_trusted_lease_time(authority, server_time) {
        return Ok(rejection_ack(message, 0, rejection));
    }

    if let Err(rejection) = validate_message_shape(message) {
        return Ok(rejection_ack(message, 0, rejection));
    }

    let (durable, job) = match load_runtime_execution_job(storage, &message.lease.job_id) {
        Ok(value) => value,
        Err(error) if error.kind() == StorageErrorKind::InvalidInput => {
            return Ok(rejection_ack(
                message,
                0,
                Rejection::Conflict("runtime event does not identify a durable ExecutionJob"),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let context = match RuntimeContext::from_durable(scope, &scope_key, &durable, &job) {
        Ok(context) => context,
        Err(rejection) => return Ok(rejection_ack(message, 0, rejection)),
    };

    let delivery_state_guard = if context.scope_kind == RuntimeScopeKind::DeliveryStage {
        let delivery_id = context.delivery_id.as_ref().ok_or_else(|| {
            RuntimeMessageError::Storage(StorageError::invalid_input(
                "Delivery runtime context has no Delivery identity",
            ))
        })?;
        let current = match load_current_delivery(storage, delivery_id) {
            Ok(delivery) => delivery,
            Err(error) if error.kind() == StorageErrorKind::InvalidInput => {
                return Ok(rejection_ack(
                    message,
                    0,
                    Rejection::Conflict("runtime event Delivery state is missing or foreign"),
                ));
            }
            Err(error) => return Err(error.into()),
        };
        if let Err(rejection) = validate_current_binding(&current, &context, message) {
            return Ok(rejection_ack(message, 0, rejection));
        }
        Some(StateRevisionGuard::new(
            delivery_stream_id(delivery_id),
            current.revision(),
        )?)
    } else if let Err(rejection) = validate_product_session_binding(&context, message) {
        return Ok(rejection_ack(message, 0, rejection));
    } else {
        None
    };
    if let Err(rejection) = validate_authority(message, authority, &context) {
        return Ok(rejection_ack(message, 0, rejection));
    }

    let stream_id = phase.stream_id.clone();
    let ledger = match load_ledger(storage, &stream_id)? {
        Some(ledger) => ledger,
        None => RuntimeLedgerState::empty(&context, &message.lease, message),
    };
    if let Err(rejection) = ledger.validate_identity(&context, &message.lease, message) {
        return Ok(rejection_ack(message, ledger.highest_sequence, rejection));
    }

    let event_digest = event_digest(&message.event)?;
    match ledger.classify(&message.event, &event_digest) {
        LedgerDecision::Duplicate => {
            return Ok(accepted_ack(
                message,
                ledger.highest_sequence,
                LeaseWriteStatus::Duplicate,
                None,
            ));
        }
        LedgerDecision::Conflict(rejection) => {
            return Ok(rejection_ack(message, ledger.highest_sequence, rejection));
        }
        LedgerDecision::Gap(next) => {
            return Ok(gap_ack(message, ledger.highest_sequence, next));
        }
        LedgerDecision::Accept => {}
    }

    let expected_revision = ledger.highest_sequence;
    let before_ledger_digest = if context.scope_kind == RuntimeScopeKind::DeliveryStage {
        Some(runtime_ledger_digest(&ledger)?)
    } else {
        None
    };
    let accepted_payload = serde_json::to_vec(message).map_err(|error| {
        RuntimeMessageError::Storage(StorageError::adapter(format!(
            "failed to encode accepted runtime event: {error}"
        )))
    })?;
    let next = ledger.append(message.event.clone(), event_digest.clone())?;
    let pending_audit_event = before_ledger_digest
        .map(|before| {
            let after = runtime_ledger_digest(&next)?;
            let state = AuditState::changed(Some(before), after).map_err(|error| {
                RuntimeMessageError::Storage(StorageError::invalid_input(format!(
                    "runtime audit state is invalid: {error}"
                )))
            })?;
            runtime_pending_audit_event(scope, &phase, &context, message, state)
        })
        .transpose()?;
    let state = serde_json::to_vec(&next).map_err(|error| {
        RuntimeMessageError::Storage(StorageError::adapter(format!(
            "failed to encode runtime ledger state: {error}"
        )))
    })?;
    let events = vec![
        NewOutboxEvent::internal(
            runtime_outbox_event_id(&stream_id, &message.event),
            RUNTIME_TOPIC,
            accepted_payload,
        ),
        runtime_projection_invalidated_event(
            &phase.receipt_identity,
            &context,
            scope,
            message,
            next.highest_sequence,
        )?,
    ];
    let mut commit = StateCommit::new(
        phase.receipt_identity.clone(),
        phase.command_digest.clone(),
        &stream_id,
        expected_revision,
        state,
        events,
    );
    if let Some(pending_audit_event) = pending_audit_event {
        commit = commit.with_pending_audit_event(pending_audit_event);
    }
    if let Some(delivery_state_guard) = delivery_state_guard {
        commit = commit.with_state_guard(delivery_state_guard);
    }
    let receipt = match storage.commit(&commit) {
        Ok(receipt) => receipt,
        Err(error) if error.is_state_guard_conflict() => {
            return Ok(rejection_ack(
                message,
                expected_revision,
                Rejection::Conflict(
                    "runtime event Delivery state changed before the event was committed",
                ),
            ));
        }
        Err(error) if error.kind() == StorageErrorKind::RequestConflict => {
            let acknowledged_sequence = prior_receipt_sequence(storage, &phase)?;
            return Ok(rejection_ack(
                message,
                acknowledged_sequence,
                Rejection::Conflict("runtime message identity was reused with different content"),
            ));
        }
        Err(error) if error.kind() == StorageErrorKind::RevisionConflict => {
            return resolve_raced_append(storage, &phase, message, &event_digest);
        }
        Err(error) => return Err(error.into()),
    };
    validate_receipt(
        &receipt,
        &phase,
        &stream_id,
        &context,
        message,
        &message.event,
    )?;
    if context.scope_kind == RuntimeScopeKind::DeliveryStage {
        validate_runtime_audit_event(storage, &phase, &receipt, message, &context)?;
    }
    let status = if receipt.idempotent_replay {
        LeaseWriteStatus::Duplicate
    } else {
        LeaseWriteStatus::Accepted
    };
    Ok(accepted_ack(message, next.highest_sequence, status, None))
}

fn validate_trusted_lease_time(
    authority: &SessionBindingAuthority,
    server_time: &Instant,
) -> Result<(), Rejection> {
    let now = instant_millis(server_time)
        .map_err(|_| Rejection::Conflict("runtime ingress time is not canonical"))?;
    let issued_at = instant_millis(authority.issued_at())
        .map_err(|_| Rejection::Conflict("runtime authority issuedAt is not canonical"))?;
    let expires_at = instant_millis(authority.expires_at())
        .map_err(|_| Rejection::Conflict("runtime authority expiresAt is not canonical"))?;
    if now < issued_at {
        return Err(Rejection::Conflict(
            "runtime ingress precedes the scheduler-owned lease",
        ));
    }
    if now >= expires_at {
        return Err(Rejection::Expired(
            "runtime ingress observed an expired scheduler-owned lease",
        ));
    }
    Ok(())
}

fn resolve_raced_append(
    storage: &dyn ProductStateStorage,
    phase: &RuntimePhase,
    message: &RuntimeEventMessage,
    digest: &Sha256Digest,
) -> Result<RuntimeAckMessage, RuntimeMessageError> {
    let Some(ledger) = load_ledger(storage, &phase.stream_id)? else {
        return Err(StorageError::revision_conflict(0, 1).into());
    };
    match ledger.classify(&message.event, digest) {
        LedgerDecision::Duplicate => Ok(accepted_ack(
            message,
            ledger.highest_sequence,
            LeaseWriteStatus::Duplicate,
            None,
        )),
        LedgerDecision::Gap(next) => Ok(gap_ack(message, ledger.highest_sequence, next)),
        LedgerDecision::Conflict(rejection) => {
            Ok(rejection_ack(message, ledger.highest_sequence, rejection))
        }
        LedgerDecision::Accept => Err(StorageError::revision_conflict(
            ledger.highest_sequence,
            ledger.highest_sequence.saturating_add(1),
        )
        .into()),
    }
}

fn prior_receipt_sequence(
    storage: &dyn ProductStateStorage,
    phase: &RuntimePhase,
) -> Result<u64, RuntimeMessageError> {
    let Some(receipt) = storage.load_receipt_for_identity(&phase.receipt_identity)? else {
        return Ok(0);
    };
    if receipt.receipt_identity != phase.receipt_identity
        || receipt.stream_id != phase.stream_id
        || receipt.events.len() != 2
        || !receipt
            .events
            .iter()
            .any(|event| event.topic == RUNTIME_TOPIC)
        || !receipt
            .events
            .iter()
            .any(|event| event.topic == RUNTIME_INVALIDATED_TOPIC)
    {
        return Err(StorageError::invalid_input(
            "runtime conflict receipt is incomplete or foreign",
        )
        .into());
    }
    let accepted_event = receipt
        .events
        .iter()
        .find(|event| event.topic == RUNTIME_TOPIC)
        .ok_or_else(|| StorageError::invalid_input("runtime conflict receipt is missing"))?;
    let accepted_message: RuntimeEventMessage = serde_json::from_slice(&accepted_event.payload)
        .map_err(|_| StorageError::invalid_input("runtime conflict receipt is not canonical"))?;
    if receipt.revision != u64::try_from(accepted_message.event.sequence.0).unwrap_or_default()
        || accepted_event.event_id
            != runtime_outbox_event_id(&phase.stream_id, &accepted_message.event)
    {
        return Err(StorageError::invalid_input(
            "runtime conflict receipt accepted event is foreign",
        )
        .into());
    }
    let invalidation = receipt
        .events
        .iter()
        .find(|event| event.topic == RUNTIME_INVALIDATED_TOPIC)
        .ok_or_else(|| {
            StorageError::invalid_input("runtime conflict receipt invalidation is missing")
        })?;
    validate_replay_invalidation(invalidation, &receipt)?;
    Ok(receipt.revision)
}

/// Loads the immutable dispatch intent without imposing the Delivery-only
/// scope restriction used by the Delivery command transaction. `ProductSession`
/// jobs use the same generated `ExecutionJob` DTO and outbox topic, but their
/// trusted state stream is the `ProductSession` stream.
fn load_runtime_execution_job(
    storage: &dyn ProductStateStorage,
    job_id: &ExecutionJobId,
) -> Result<(DurableOutboxEvent, ExecutionJob), StorageError> {
    let event_id = format!("execution-job:{}", job_id.0);
    let durable = storage
        .load_outbox_event(&event_id)?
        .ok_or_else(|| StorageError::invalid_input("ExecutionJob event does not exist"))?;
    let event = durable.event();
    if event.event_id != event_id
        || event.topic != EXECUTION_JOB_TOPIC
        || event.projection_cursor.is_some()
    {
        return Err(StorageError::invalid_input(
            "durable event is not the exact internal ExecutionJob intent",
        ));
    }
    let job: ExecutionJob = serde_json::from_slice(&event.payload).map_err(|error| {
        StorageError::invalid_input(format!("durable ExecutionJob payload is invalid: {error}"))
    })?;
    let canonical = serde_json::to_vec(&job).map_err(|error| {
        StorageError::adapter(format!("failed to encode durable ExecutionJob: {error}"))
    })?;
    if canonical != event.payload || &job.job_id != job_id {
        return Err(StorageError::invalid_input(
            "durable ExecutionJob event identity or payload is not canonical",
        ));
    }
    Ok((durable, job))
}

struct RuntimePhase {
    receipt_identity: ReceiptIdentity,
    command_digest: Sha256Digest,
    stream_id: String,
}

impl RuntimePhase {
    fn new(
        scope_key: &ReceiptScopeKey,
        message: &RuntimeEventMessage,
    ) -> Result<Self, StorageError> {
        // ProductSession runtime facts have one stable resource stream so the
        // StrongFlow read-cut can join the ledger with the ProductSession
        // event cursor from only the requested session id. Delivery runtime
        // facts remain isolated by repository scope and ExecutionJob id.
        let stream_id = if message.session_identity.stage_run_id.is_none() {
            product_session_runtime_stream_id(&message.session_identity.product_session_id)
        } else {
            runtime_stream_id(scope_key, &message.lease.job_id)
        };
        let receipt_identity = ReceiptIdentity::new(
            execution_message_actor_key(&message.message_id)?,
            scope_key.clone(),
            execution_message_request_id(&message.message_id, RUNTIME_PHASE)?,
        )?;
        let encoded = serde_json::to_vec(&message.event).map_err(|error| {
            StorageError::adapter(format!("failed to encode runtime event record: {error}"))
        })?;
        let command_digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(encoded)));
        Ok(Self {
            receipt_identity,
            command_digest,
            stream_id,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RuntimeLedgerState {
    pub(crate) schema_version: u8,
    pub(crate) delivery_id: Option<DeliveryId>,
    pub(crate) delivery_task_id: Option<DeliveryTaskId>,
    pub(crate) stage_run_id: Option<StageRunId>,
    pub(crate) product_session_id: ProductSessionId,
    pub(crate) execution_job_id: ExecutionJobId,
    pub(crate) worker_session_id: WorkerSessionId,
    pub(crate) codex_thread_id: CodexThreadId,
    pub(crate) lease_id: LeaseId,
    pub(crate) attempt: u64,
    pub(crate) fencing_token: FencingToken,
    pub(crate) worker_id: WorkerId,
    pub(crate) worker_instance_id: WorkerInstanceId,
    pub(crate) highest_sequence: u64,
    pub(crate) events: Vec<RuntimeLedgerEvent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RuntimeLedgerEvent {
    pub(crate) event: ExecutionEventRecord,
    pub(crate) event_digest: Sha256Digest,
}

impl RuntimeLedgerState {
    fn empty(
        context: &RuntimeContext,
        lease: &ExecutionLeaseStamp,
        message: &RuntimeEventMessage,
    ) -> Self {
        Self {
            schema_version: RUNTIME_STATE_VERSION,
            delivery_id: context.delivery_id.clone(),
            delivery_task_id: context.delivery_task_id.clone(),
            stage_run_id: context.stage_run_id.clone(),
            product_session_id: context.product_session_id.clone(),
            execution_job_id: context.execution_job_id.clone(),
            worker_session_id: message.worker_session_id.clone(),
            codex_thread_id: message.codex_thread_id.clone(),
            lease_id: lease.lease_id.clone(),
            attempt: u64::try_from(lease.attempt).unwrap_or_default(),
            fencing_token: lease.fencing_token.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            highest_sequence: 0,
            events: Vec::new(),
        }
    }

    fn validate_identity(
        &self,
        context: &RuntimeContext,
        lease: &ExecutionLeaseStamp,
        message: &RuntimeEventMessage,
    ) -> Result<(), Rejection> {
        let attempt = u64::try_from(lease.attempt).map_err(|_| {
            Rejection::Conflict("runtime lease attempt is outside the accepted range")
        })?;
        if self.schema_version != RUNTIME_STATE_VERSION
            || (context.scope_kind == RuntimeScopeKind::DeliveryStage) != self.delivery_id.is_some()
            || (context.scope_kind == RuntimeScopeKind::DeliveryStage)
                != self.stage_run_id.is_some()
            || self.delivery_id != context.delivery_id
            || self.delivery_task_id != context.delivery_task_id
            || self.stage_run_id != context.stage_run_id
            || self.product_session_id != context.product_session_id
            || self.execution_job_id != context.execution_job_id
            || self.worker_session_id != message.worker_session_id
            || self.codex_thread_id != message.codex_thread_id
            || self.lease_id != lease.lease_id
            || self.attempt != attempt
            || self.fencing_token != lease.fencing_token
            || self.worker_id != lease.worker_id
            || self.worker_instance_id != lease.worker_instance_id
        {
            return Err(Rejection::Conflict(
                "runtime ledger identity is foreign or stale",
            ));
        }
        Ok(())
    }

    fn classify(&self, event: &ExecutionEventRecord, digest: &Sha256Digest) -> LedgerDecision {
        if let Some(existing) = self
            .events
            .iter()
            .find(|existing| existing.event.event_id == event.event_id)
        {
            if existing.event.sequence == event.sequence && existing.event_digest == *digest {
                return LedgerDecision::Duplicate;
            }
            return LedgerDecision::Conflict(Rejection::Conflict(
                "runtime event identity was reused with different content",
            ));
        }
        if let Some(existing) = self
            .events
            .iter()
            .find(|existing| existing.event.sequence == event.sequence)
        {
            return LedgerDecision::Conflict(Rejection::Conflict(
                if existing.event_digest == *digest {
                    "runtime sequence was already accepted with another event identity"
                } else {
                    "runtime sequence was already accepted with different content"
                },
            ));
        }
        let expected = self.highest_sequence.saturating_add(1);
        let sequence = u64::try_from(event.sequence.0).unwrap_or(u64::MAX);
        if sequence > expected {
            return LedgerDecision::Gap(expected);
        }
        if sequence < expected {
            return LedgerDecision::Conflict(Rejection::Conflict(
                "runtime event is older than the current contiguous sequence",
            ));
        }
        LedgerDecision::Accept
    }

    fn append(
        mut self,
        event: ExecutionEventRecord,
        event_digest: Sha256Digest,
    ) -> Result<Self, RuntimeMessageError> {
        let sequence = u64::try_from(event.sequence.0).map_err(|_| {
            RuntimeMessageError::Storage(StorageError::invalid_input(
                "runtime ledger sequence is outside the accepted range",
            ))
        })?;
        if sequence != self.highest_sequence.saturating_add(1) {
            return Err(RuntimeMessageError::Storage(StorageError::invalid_input(
                "runtime ledger append is not contiguous",
            )));
        }
        self.highest_sequence = sequence;
        self.events.push(RuntimeLedgerEvent {
            event,
            event_digest,
        });
        Ok(self)
    }
}

enum LedgerDecision {
    Accept,
    Duplicate,
    Gap(u64),
    Conflict(Rejection),
}

#[derive(Clone, Copy)]
enum Rejection {
    Conflict(&'static str),
    Expired(&'static str),
    StaleFencingToken(&'static str),
    WorkerInstance(&'static str),
}

impl Rejection {
    fn status(self) -> LeaseWriteStatus {
        match self {
            Self::Conflict(_) => LeaseWriteStatus::RejectedConflict,
            Self::Expired(_) => LeaseWriteStatus::RejectedExpiredLease,
            Self::StaleFencingToken(_) => LeaseWriteStatus::RejectedStaleFencingToken,
            Self::WorkerInstance(_) => LeaseWriteStatus::RejectedWorkerInstance,
        }
    }

    fn code(self) -> ExecutionPortErrorCode {
        match self {
            Self::Conflict(_) => ExecutionPortErrorCode::MessageConflict,
            Self::Expired(_) => ExecutionPortErrorCode::LeaseExpired,
            Self::StaleFencingToken(_) => ExecutionPortErrorCode::StaleFencingToken,
            Self::WorkerInstance(_) => ExecutionPortErrorCode::WorkerInstanceChanged,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Conflict(message)
            | Self::Expired(message)
            | Self::StaleFencingToken(message)
            | Self::WorkerInstance(message) => message,
        }
    }

    fn retryable(self) -> bool {
        matches!(
            self,
            Self::Expired(_) | Self::StaleFencingToken(_) | Self::WorkerInstance(_)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeScopeKind {
    DeliveryStage,
    ProductSession,
}

struct RuntimeContext {
    scope_kind: RuntimeScopeKind,
    delivery_id: Option<DeliveryId>,
    delivery_task_id: Option<DeliveryTaskId>,
    stage_run_id: Option<StageRunId>,
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    attempt: u64,
}

impl RuntimeContext {
    fn from_durable(
        scope: &RepositoryScope,
        scope_key: &ReceiptScopeKey,
        durable: &DurableOutboxEvent,
        job: &ExecutionJob,
    ) -> Result<Self, Rejection> {
        let (
            scope_kind,
            delivery_id,
            delivery_task_id,
            stage_run_id,
            product_session_id,
            intent_stream_id,
        ) = match &job.scope {
            ExecutionScope::DeliveryStageExecutionScope(job_scope) => {
                if job_scope.kind != DeliveryStageExecutionScopeKind::DeliveryStage {
                    return Err(Rejection::Conflict(
                        "runtime event Delivery ExecutionJob scope discriminator is not canonical",
                    ));
                }
                (
                    RuntimeScopeKind::DeliveryStage,
                    Some(job_scope.delivery_id.clone()),
                    job_scope.delivery_task_id.clone(),
                    Some(job_scope.stage_run_id.clone()),
                    job_scope.product_session_id.clone(),
                    delivery_stream_id(&job_scope.delivery_id),
                )
            }
            ExecutionScope::ProductSessionExecutionScope(job_scope) => {
                if job_scope.kind != ProductSessionExecutionScopeKind::ProductSession {
                    return Err(Rejection::Conflict(
                        "runtime event ProductSession ExecutionJob scope discriminator is not canonical",
                    ));
                }
                (
                    RuntimeScopeKind::ProductSession,
                    None,
                    None,
                    None,
                    job_scope.product_session_id.clone(),
                    catalog_stream_id(scope_key),
                )
            }
        };
        if durable.receipt_identity().scope_key() != scope_key
            || durable.stream_id() != intent_stream_id
            || durable.revision() == 0
            || job.workspace.repository_id != scope.repository_id
        {
            return Err(Rejection::Conflict(
                "runtime event ExecutionJob does not belong to the trusted repository scope",
            ));
        }
        let attempt = u64::try_from(job.attempt)
            .map_err(|_| Rejection::Conflict("runtime ExecutionJob attempt is out of range"))?;
        if !(1..=1_000).contains(&attempt) {
            return Err(Rejection::Conflict(
                "runtime ExecutionJob attempt is outside the accepted range",
            ));
        }
        Ok(Self {
            scope_kind,
            delivery_id,
            delivery_task_id,
            stage_run_id,
            product_session_id,
            execution_job_id: job.job_id.clone(),
            attempt,
        })
    }
}

fn load_current_delivery(
    storage: &dyn ProductStateStorage,
    delivery_id: &DeliveryId,
) -> Result<Delivery, StorageError> {
    let stream_id = delivery_stream_id(delivery_id);
    let state = storage
        .load_state(&stream_id)?
        .ok_or_else(|| StorageError::invalid_input("runtime event Delivery state is missing"))?;
    if state.stream_id != stream_id || state.revision == 0 {
        return Err(StorageError::invalid_input(
            "runtime event Delivery state is foreign",
        ));
    }
    let delivery = Delivery::decode_json(&state.payload).map_err(|error| {
        StorageError::invalid_input(format!("runtime event Delivery is invalid: {error}"))
    })?;
    if delivery.id() != delivery_id || delivery.revision() != state.revision {
        return Err(StorageError::invalid_input(
            "runtime event Delivery snapshot differs from durable state",
        ));
    }
    Ok(delivery)
}

/// `ProductSession` execution has no hidden Delivery or `StageRun` aggregate.
/// Its trusted join is the immutable ProductSession-scoped dispatch intent
/// plus the scheduler lease; the generated message carries the canonical
/// `ProductSession` identity and must omit the Delivery-only `StageRun` attachment.
fn validate_product_session_binding(
    context: &RuntimeContext,
    message: &RuntimeEventMessage,
) -> Result<(), Rejection> {
    if context.scope_kind != RuntimeScopeKind::ProductSession
        || context.delivery_id.is_some()
        || context.delivery_task_id.is_some()
        || context.stage_run_id.is_some()
        || message.session_identity.product_session_id != context.product_session_id
        || message.session_identity.stage_run_id.is_some()
        || message.session_identity.worker_session_id != message.worker_session_id
        || message.session_identity.codex_thread_id != message.codex_thread_id
    {
        return Err(Rejection::Conflict(
            "runtime event does not match the exact ProductSession binding",
        ));
    }
    Ok(())
}

fn validate_current_binding(
    delivery: &Delivery,
    context: &RuntimeContext,
    message: &RuntimeEventMessage,
) -> Result<(), Rejection> {
    let (Some(delivery_id), Some(stage_run_id)) =
        (context.delivery_id.as_ref(), context.stage_run_id.as_ref())
    else {
        return Err(Rejection::Conflict(
            "Delivery runtime event has no Delivery-stage identity",
        ));
    };
    let matches = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| {
            binding.delivery_id == *delivery_id
                && binding.delivery_task_id == context.delivery_task_id
                && binding.stage_run_id == *stage_run_id
                && binding.product_session_id == context.product_session_id
                && binding.execution_job_id == context.execution_job_id
        })
        .collect::<Vec<_>>();
    let [binding] = matches.as_slice() else {
        return Err(Rejection::Conflict(
            "runtime event does not match exactly one current SessionBinding",
        ));
    };
    let runs = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| {
            run.id == *stage_run_id
                && run.delivery_id == *delivery_id
                && run.delivery_task_id == context.delivery_task_id
                && run.attempt == context.attempt
                && matches!(
                    run.status,
                    StageRunStatus::Running | StageRunStatus::Waiting
                )
        })
        .count();
    if runs != 1
        || message.session_identity.product_session_id != context.product_session_id
        || message.session_identity.stage_run_id.as_ref() != Some(stage_run_id)
        || message.session_identity.worker_session_id != message.worker_session_id
        || message.session_identity.codex_thread_id != message.codex_thread_id
        || binding.worker_session_id.as_ref() != Some(&message.worker_session_id)
        || binding.codex_thread_id.as_ref() != Some(&message.codex_thread_id)
        || binding.attempt != context.attempt
        || binding
            .lease_id
            .as_ref()
            .is_some_and(|value| value != &message.lease.lease_id)
        || binding
            .worker_id
            .as_ref()
            .is_some_and(|value| value != &message.lease.worker_id)
        || binding
            .worker_instance_id
            .as_ref()
            .is_some_and(|value| value != &message.lease.worker_instance_id)
        || binding
            .fencing_token
            .as_ref()
            .is_some_and(|value| value != &message.lease.fencing_token)
    {
        return Err(Rejection::Conflict(
            "runtime event requires the current WorkerSession and CodexThread binding",
        ));
    }
    Ok(())
}

fn validate_authority(
    message: &RuntimeEventMessage,
    authority: &SessionBindingAuthority,
    context: &RuntimeContext,
) -> Result<(), Rejection> {
    let active = authority.active_lease();
    let attempt = u64::try_from(message.lease.attempt)
        .map_err(|_| Rejection::Conflict("runtime lease attempt is out of range"))?;
    if context.attempt != attempt {
        return Err(Rejection::Conflict(
            "runtime event attempt does not match the durable ExecutionJob",
        ));
    }
    if active.execution_job_id() != &message.lease.job_id
        || active.attempt() != attempt
        || active.lease_id() != &message.lease.lease_id
        || active.worker_id() != &message.lease.worker_id
        || active.worker_session_id() != &message.worker_session_id
        || context.execution_job_id != message.lease.job_id
    {
        return Err(Rejection::Conflict(
            "runtime event does not match the scheduler-owned lease",
        ));
    }
    if active.worker_instance_id() != &message.lease.worker_instance_id {
        return Err(Rejection::WorkerInstance(
            "runtime event uses a different Worker instance",
        ));
    }
    if active.fencing_token() != &message.lease.fencing_token {
        if decimal_token(&message.lease.fencing_token) < decimal_token(active.fencing_token()) {
            return Err(Rejection::StaleFencingToken(
                "runtime event uses a stale fencing token",
            ));
        }
        return Err(Rejection::Conflict(
            "runtime event fencing token is not scheduler-owned",
        ));
    }
    if authority.issued_at() != &message.lease.issued_at
        || authority.expires_at() != &message.lease.expires_at
    {
        return Err(Rejection::Conflict(
            "runtime event changed the scheduler-owned lease window",
        ));
    }
    let sent_at = instant_millis(&message.sent_at)
        .map_err(|_| Rejection::Conflict("runtime event sentAt is not canonical"))?;
    let issued_at = instant_millis(&message.lease.issued_at)
        .map_err(|_| Rejection::Conflict("runtime event issuedAt is not canonical"))?;
    let expires_at = instant_millis(&message.lease.expires_at)
        .map_err(|_| Rejection::Conflict("runtime event expiresAt is not canonical"))?;
    let occurred_at = instant_millis(&message.event.occurred_at)
        .map_err(|_| Rejection::Conflict("runtime event occurredAt is not canonical"))?;
    if sent_at < issued_at {
        return Err(Rejection::Conflict(
            "runtime event sentAt precedes its lease",
        ));
    }
    if sent_at >= expires_at {
        return Err(Rejection::Expired("runtime event lease has expired"));
    }
    if occurred_at < issued_at || occurred_at > sent_at {
        return Err(Rejection::Conflict(
            "runtime event occurredAt is outside its message window",
        ));
    }
    Ok(())
}

fn validate_message_shape(message: &RuntimeEventMessage) -> Result<(), Rejection> {
    if message.kind != RuntimeEventMessageKind::RuntimeEvent
        || message.schema_version != SchemaVersion::WinwincodeV1
    {
        return Err(Rejection::Conflict(
            "runtime event discriminator is not canonical",
        ));
    }
    require_id(&message.message_id.0, "xmsg_", "messageId")
        .map_err(|_| Rejection::Conflict("runtime event messageId is not canonical"))?;
    require_id(&message.event.event_id.0, "xevt_", "event.eventId")
        .map_err(|_| Rejection::Conflict("runtime event eventId is not canonical"))?;
    require_id(&message.lease.job_id.0, "job_", "lease.jobId")
        .map_err(|_| Rejection::Conflict("runtime event lease.jobId is not canonical"))?;
    require_id(&message.lease.lease_id.0, "lse_", "lease.leaseId")
        .map_err(|_| Rejection::Conflict("runtime event lease.leaseId is not canonical"))?;
    require_id(&message.lease.worker_id.0, "wrk_", "lease.workerId")
        .map_err(|_| Rejection::Conflict("runtime event lease.workerId is not canonical"))?;
    require_id(
        &message.lease.worker_instance_id.0,
        "wki_",
        "lease.workerInstanceId",
    )
    .map_err(|_| Rejection::Conflict("runtime event workerInstanceId is not canonical"))?;
    require_id(&message.worker_session_id.0, "wsn_", "workerSessionId")
        .map_err(|_| Rejection::Conflict("runtime event workerSessionId is not canonical"))?;
    require_id(&message.codex_thread_id.0, "cdx_", "codexThreadId")
        .map_err(|_| Rejection::Conflict("runtime event codexThreadId is not canonical"))?;
    require_id(
        &message.session_identity.product_session_id.0,
        "psn_",
        "sessionIdentity.productSessionId",
    )
    .map_err(|_| Rejection::Conflict("runtime event session productSessionId is not canonical"))?;
    if let Some(stage_run_id) = message.session_identity.stage_run_id.as_ref() {
        require_id(&stage_run_id.0, "run_", "sessionIdentity.stageRunId").map_err(|_| {
            Rejection::Conflict("runtime event session stageRunId is not canonical")
        })?;
    }
    require_id(
        &message.session_identity.worker_session_id.0,
        "wsn_",
        "sessionIdentity.workerSessionId",
    )
    .map_err(|_| Rejection::Conflict("runtime event session workerSessionId is not canonical"))?;
    require_id(
        &message.session_identity.codex_thread_id.0,
        "cdx_",
        "sessionIdentity.codexThreadId",
    )
    .map_err(|_| Rejection::Conflict("runtime event session codexThreadId is not canonical"))?;
    if !(1..=1_000).contains(&message.lease.attempt)
        || message.lease.fencing_token.0.is_empty()
        || message.lease.fencing_token.0.len() > 20
        || message.lease.fencing_token.0.starts_with('0')
        || !message
            .lease
            .fencing_token
            .0
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(Rejection::Conflict(
            "runtime event lease attempt or fencingToken is invalid",
        ));
    }
    let sequence = u64::try_from(message.event.sequence.0)
        .map_err(|_| Rejection::Conflict("runtime event sequence is negative"))?;
    if sequence == 0 || sequence > MAX_SAFE_INTEGER {
        return Err(Rejection::Conflict(
            "runtime event sequence is outside the accepted range",
        ));
    }
    if message.event.summary.is_empty()
        || message.event.summary.chars().count() > MAX_SUMMARY_LENGTH
    {
        return Err(Rejection::Conflict(
            "runtime event summary is outside the accepted range",
        ));
    }
    instant_millis(&message.lease.issued_at)
        .map_err(|_| Rejection::Conflict("runtime event issuedAt is not canonical"))?;
    instant_millis(&message.lease.expires_at)
        .map_err(|_| Rejection::Conflict("runtime event expiresAt is not canonical"))?;
    instant_millis(&message.sent_at)
        .map_err(|_| Rejection::Conflict("runtime event sentAt is not canonical"))?;
    instant_millis(&message.event.occurred_at)
        .map_err(|_| Rejection::Conflict("runtime event occurredAt is not canonical"))?;
    Ok(())
}

fn load_ledger(
    storage: &dyn ProductStateStorage,
    stream_id: &str,
) -> Result<Option<RuntimeLedgerState>, StorageError> {
    let Some(state) = storage.load_state(stream_id)? else {
        return Ok(None);
    };
    decode_runtime_ledger_state(&state, stream_id).map(Some)
}

pub(crate) fn decode_runtime_ledger_state(
    state: &winwincode_storage::StoredState,
    stream_id: &str,
) -> Result<RuntimeLedgerState, StorageError> {
    if state.stream_id != stream_id || state.revision == 0 {
        return Err(StorageError::invalid_input(
            "runtime ledger state is foreign",
        ));
    }
    let ledger: RuntimeLedgerState = serde_json::from_slice(&state.payload).map_err(|error| {
        StorageError::invalid_input(format!("runtime ledger state is invalid: {error}"))
    })?;
    if ledger.highest_sequence != ledger.events.len() as u64
        || state.revision != ledger.highest_sequence
    {
        return Err(StorageError::invalid_input(
            "runtime ledger state sequence is not canonical",
        ));
    }
    for (index, entry) in ledger.events.iter().enumerate() {
        let expected_sequence = u64::try_from(index + 1)
            .map_err(|_| StorageError::adapter("runtime ledger sequence is out of range"))?;
        let sequence = u64::try_from(entry.event.sequence.0)
            .map_err(|_| StorageError::invalid_input("runtime ledger sequence is negative"))?;
        let event_encoded = serde_json::to_vec(&entry.event).map_err(|error| {
            StorageError::adapter(format!(
                "failed to encode runtime ledger replay event: {error}"
            ))
        })?;
        let digest = Sha256Digest(format!("sha256:{:x}", Sha256::digest(event_encoded)));
        if sequence != expected_sequence || digest != entry.event_digest {
            return Err(StorageError::invalid_input(
                "runtime ledger event sequence or digest is not canonical",
            ));
        }
    }
    Ok(ledger)
}

fn event_digest(event: &ExecutionEventRecord) -> Result<Sha256Digest, StorageError> {
    let encoded = serde_json::to_vec(event).map_err(|error| {
        StorageError::adapter(format!("failed to encode runtime event digest: {error}"))
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn runtime_ledger_digest(ledger: &RuntimeLedgerState) -> Result<Sha256Digest, RuntimeMessageError> {
    let encoded = serde_json::to_vec(ledger).map_err(|error| {
        RuntimeMessageError::Storage(StorageError::adapter(format!(
            "failed to encode runtime ledger digest: {error}"
        )))
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn runtime_pending_audit_event(
    scope: &RepositoryScope,
    phase: &RuntimePhase,
    context: &RuntimeContext,
    message: &RuntimeEventMessage,
    state: AuditState,
) -> Result<PendingAuditEvent, RuntimeMessageError> {
    let delivery_id = context.delivery_id.clone().ok_or_else(|| {
        RuntimeMessageError::Storage(StorageError::invalid_input(
            "Delivery runtime audit event has no Delivery identity",
        ))
    })?;
    let stage_run_id = context.stage_run_id.clone().ok_or_else(|| {
        RuntimeMessageError::Storage(StorageError::invalid_input(
            "Delivery runtime audit event has no StageRun identity",
        ))
    })?;
    let attempt = u64::try_from(message.lease.attempt).map_err(|_| {
        RuntimeMessageError::Storage(StorageError::invalid_input(
            "runtime audit event attempt is out of range",
        ))
    })?;
    let identity = AuditExecutionIdentity::try_new(
        context.product_session_id.clone(),
        message.worker_session_id.clone(),
        message.codex_thread_id.clone(),
        stage_run_id,
        context.execution_job_id.clone(),
        delivery_id,
        context.delivery_task_id.clone(),
        message.lease.worker_id.clone(),
        message.lease.worker_instance_id.clone(),
        message.lease.lease_id.clone(),
        attempt,
        message.lease.fencing_token.clone(),
        ExecutionAckSequence(message.event.sequence.0),
    )
    .map_err(|error| {
        RuntimeMessageError::Storage(StorageError::invalid_input(format!(
            "runtime audit identity is invalid: {error}"
        )))
    })?;
    let event_id = AuditEventId::from_digest(&phase.command_digest).map_err(|error| {
        RuntimeMessageError::Storage(StorageError::invalid_input(format!(
            "runtime audit event id is invalid: {error}"
        )))
    })?;
    let action = AuditAction::worker_lease("runtime.event.accepted").map_err(|error| {
        RuntimeMessageError::Storage(StorageError::invalid_input(format!(
            "runtime audit action is invalid: {error}"
        )))
    })?;
    let occurred_at_millis = instant_millis(&message.event.occurred_at)?;
    execution_audit_event_with_state(
        event_id,
        occurred_at_millis,
        phase.receipt_identity.request_id().clone(),
        scope,
        action,
        state,
        AuditSubject::runtime(identity),
        "execution.runtime.accepted",
    )
    .map_err(RuntimeMessageError::Storage)
}

fn validate_runtime_audit_event(
    storage: &dyn ProductStateStorage,
    phase: &RuntimePhase,
    receipt: &CommitReceipt,
    message: &RuntimeEventMessage,
    context: &RuntimeContext,
) -> Result<(), StorageError> {
    let pending = storage
        .load_pending_audit_event(&phase.receipt_identity)?
        .ok_or_else(|| {
            StorageError::invalid_input("runtime receipt is missing its canonical audit event")
        })?;
    let event: AuditEvent = serde_json::from_slice(pending.payload()).map_err(|_| {
        StorageError::invalid_input("runtime audit event payload is not canonical JSON")
    })?;
    let canonical = serde_json::to_vec(&event)
        .map_err(|_| StorageError::adapter("runtime audit event cannot be canonically encoded"))?;
    let expected_event_id = AuditEventId::from_digest(&phase.command_digest)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if canonical != pending.payload()
        || event.event_id().as_str() != pending.event_id()
        || event.event_id() != &expected_event_id
        || event.request_id() != phase.receipt_identity.request_id()
        || event.occurred_at_millis() != instant_millis(&message.event.occurred_at)?
        || event.result_code() != "execution.runtime.accepted"
        || event.subject().execution_kind() != Some(AuditExecutionSubjectKind::Runtime)
    {
        return Err(StorageError::invalid_input(
            "runtime audit event does not match its accepted receipt",
        ));
    }
    let expected_scope = repository_scope_from_receipt_key(phase.receipt_identity.scope_key())?;
    let expected_scope = AuditScope::repository(
        expected_scope.organization_id,
        expected_scope.workspace_id,
        expected_scope.project_id,
        expected_scope.repository_id,
    )
    .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    if event.scope() != &expected_scope {
        return Err(StorageError::invalid_input(
            "runtime audit event scope is foreign",
        ));
    }
    let identity = event.subject().execution().ok_or_else(|| {
        StorageError::invalid_input("runtime audit event has no execution identity")
    })?;
    let attempt = u64::try_from(message.lease.attempt)
        .map_err(|_| StorageError::invalid_input("runtime audit attempt is out of range"))?;
    let expected_sequence = ExecutionAckSequence(message.event.sequence.0);
    if identity.product_session_id() != &context.product_session_id
        || identity.worker_session_id() != &message.worker_session_id
        || identity.codex_thread_id() != &message.codex_thread_id
        || identity.stage_run_id()
            != context.stage_run_id.as_ref().ok_or_else(|| {
                StorageError::invalid_input("runtime audit context has no StageRun identity")
            })?
        || identity.execution_job_id() != &context.execution_job_id
        || identity.delivery_id()
            != context.delivery_id.as_ref().ok_or_else(|| {
                StorageError::invalid_input("runtime audit context has no Delivery identity")
            })?
        || identity.delivery_task_id() != context.delivery_task_id.as_ref()
        || identity.worker_id() != &message.lease.worker_id
        || identity.worker_instance_id() != &message.lease.worker_instance_id
        || identity.lease_id() != &message.lease.lease_id
        || identity.attempt() != attempt
        || identity.fencing_token() != &message.lease.fencing_token
        || identity.source_sequence() != Some(&expected_sequence)
        || identity.binding_source().is_some()
    {
        return Err(StorageError::invalid_input(
            "runtime audit execution identity is foreign or stale",
        ));
    }
    if receipt
        .events
        .iter()
        .all(|event| event.event_id != runtime_outbox_event_id(&phase.stream_id, &message.event))
    {
        return Err(StorageError::invalid_input(
            "runtime audit receipt does not contain its accepted event",
        ));
    }
    Ok(())
}

fn validate_receipt(
    receipt: &CommitReceipt,
    phase: &RuntimePhase,
    stream_id: &str,
    context: &RuntimeContext,
    message: &RuntimeEventMessage,
    event: &ExecutionEventRecord,
) -> Result<(), RuntimeMessageError> {
    if receipt.receipt_identity != phase.receipt_identity
        || receipt.command_digest != phase.command_digest
        || receipt.stream_id != stream_id
        || receipt.revision != u64::try_from(event.sequence.0).unwrap_or_default()
        || receipt.events.len() != 2
        || !receipt
            .events
            .iter()
            .any(|event| event.topic == RUNTIME_TOPIC)
        || !receipt
            .events
            .iter()
            .any(|event| event.topic == RUNTIME_INVALIDATED_TOPIC)
    {
        return Err(RuntimeMessageError::Storage(StorageError::invalid_input(
            "runtime event durable receipt is incomplete or foreign",
        )));
    }
    let accepted_event = receipt
        .events
        .iter()
        .find(|event| event.topic == RUNTIME_TOPIC)
        .ok_or_else(|| {
            RuntimeMessageError::Storage(StorageError::invalid_input(
                "runtime event accepted receipt is missing its event",
            ))
        })?;
    let accepted_message: RuntimeEventMessage = serde_json::from_slice(&accepted_event.payload)
        .map_err(|_| {
            RuntimeMessageError::Storage(StorageError::invalid_input(
                "runtime event accepted receipt payload is not canonical",
            ))
        })?;
    if accepted_message.event.event_id != message.event.event_id
        || accepted_message.event.sequence != message.event.sequence
        || event_digest(&accepted_message.event)? != phase.command_digest
        || accepted_event.event_id != runtime_outbox_event_id(stream_id, event)
    {
        return Err(RuntimeMessageError::Storage(StorageError::invalid_input(
            "runtime event accepted receipt payload is foreign",
        )));
    }
    let invalidation = receipt
        .events
        .iter()
        .find(|event| event.topic == RUNTIME_INVALIDATED_TOPIC)
        .ok_or_else(|| {
            RuntimeMessageError::Storage(StorageError::invalid_input(
                "runtime event receipt is missing its projection invalidation",
            ))
        })?;
    validate_runtime_invalidation(
        invalidation,
        receipt,
        context,
        &message.session_identity,
        receipt.revision,
    )?;
    Ok(())
}

fn replay_ack(
    receipt: &CommitReceipt,
    phase: &RuntimePhase,
    message: &RuntimeEventMessage,
) -> Result<RuntimeAckMessage, StorageError> {
    if receipt.receipt_identity != phase.receipt_identity
        || receipt.command_digest != phase.command_digest
        || receipt.stream_id != phase.stream_id
        || receipt.events.len() != 2
        || !receipt
            .events
            .iter()
            .any(|event| event.topic == RUNTIME_TOPIC)
        || !receipt
            .events
            .iter()
            .any(|event| event.topic == RUNTIME_INVALIDATED_TOPIC)
    {
        return Err(StorageError::invalid_input(
            "runtime event replay receipt is incomplete or foreign",
        ));
    }
    let accepted_event = receipt
        .events
        .iter()
        .find(|event| event.topic == RUNTIME_TOPIC)
        .ok_or_else(|| StorageError::invalid_input("runtime event replay payload is missing"))?;
    let accepted_message: RuntimeEventMessage = serde_json::from_slice(&accepted_event.payload)
        .map_err(|_| {
            StorageError::invalid_input("runtime event replay payload is not canonical")
        })?;
    if accepted_message.event.event_id != message.event.event_id
        || accepted_message.event.sequence != message.event.sequence
        || event_digest(&accepted_message.event)? != phase.command_digest
    {
        return Err(StorageError::invalid_input(
            "runtime event replay payload does not match the request",
        ));
    }
    if receipt.revision != u64::try_from(accepted_message.event.sequence.0).unwrap_or_default()
        || accepted_event.event_id
            != runtime_outbox_event_id(&phase.stream_id, &accepted_message.event)
    {
        return Err(StorageError::invalid_input(
            "runtime event replay accepted event is foreign",
        ));
    }
    let invalidation = receipt
        .events
        .iter()
        .find(|event| event.topic == RUNTIME_INVALIDATED_TOPIC)
        .ok_or_else(|| {
            StorageError::invalid_input("runtime event replay invalidation is missing")
        })?;
    validate_replay_invalidation(invalidation, receipt)?;
    Ok(accepted_ack(
        message,
        receipt.revision,
        LeaseWriteStatus::Duplicate,
        None,
    ))
}

#[allow(clippy::too_many_lines)]
fn validate_runtime_invalidation(
    event: &winwincode_storage::OutboxEvent,
    receipt: &CommitReceipt,
    context: &RuntimeContext,
    session_identity: &SessionIdentity,
    accepted_sequence: u64,
) -> Result<(), StorageError> {
    let expected_revision = i64::try_from(accepted_sequence)
        .map(Revision)
        .map_err(|_| StorageError::invalid_input("runtime projection revision is out of range"))?;
    let expected_sequence = i64::try_from(accepted_sequence)
        .map_err(|_| StorageError::invalid_input("runtime projection sequence is out of range"))?;
    let cursor = event.projection_cursor.as_ref().ok_or_else(|| {
        StorageError::invalid_input("runtime projection invalidation has no projection cursor")
    })?;
    if cursor.sequence() == 0
        || cursor.key().scope_key() != receipt.receipt_identity.scope_key()
        || cursor.event_id().map(|id| id.0.as_str()) != Some(event.event_id.as_str())
        || event.topic != RUNTIME_INVALIDATED_TOPIC
    {
        return Err(StorageError::invalid_input(
            "runtime projection invalidation cursor is foreign",
        ));
    }
    let payload_event_id = projection_event_id(
        RUNTIME_INVALIDATION_NAMESPACE,
        receipt.receipt_identity.scope_key(),
        &event.payload,
    );
    if payload_event_id.0 != event.event_id {
        return Err(StorageError::invalid_input(
            "runtime projection invalidation event id is not canonical",
        ));
    }
    match context.scope_kind {
        RuntimeScopeKind::DeliveryStage => {
            let payload: ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent =
                serde_json::from_slice(&event.payload).map_err(|_| {
                    StorageError::invalid_input(
                        "Delivery runtime projection invalidation is not canonical",
                    )
                })?;
            let delivery_id = context.delivery_id.as_ref().ok_or_else(|| {
                StorageError::invalid_input("Delivery runtime context has no Delivery id")
            })?;
            let stage_run_id = context.stage_run_id.as_ref().ok_or_else(|| {
                StorageError::invalid_input("Delivery runtime context has no StageRun id")
            })?;
            let canonical = serde_json::to_vec(&payload).map_err(|error| {
                StorageError::adapter(format!(
                    "failed to encode Delivery runtime invalidation: {error}"
                ))
            })?;
            if canonical != event.payload
                || payload.delivery_id != *delivery_id
                || payload.stage_run_id != *stage_run_id
                || payload.product_session_id != context.product_session_id
                || payload.projection_revision != expected_revision
                || payload.last_projection_sequence != expected_sequence
                || payload.session_identity != *session_identity
                || payload.reload_queries
                    != (
                        ControlPlaneWebSocketDeliveryGetReloadQuery::DeliveryGet,
                        ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
                    )
                || payload.scope_kind
                    != ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind::DeliveryStage
                || payload.type_value
                    != ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1
                || cursor.key().stream()
                    != &ProjectionEventStream::Delivery(delivery_id.clone())
            {
                return Err(StorageError::invalid_input(
                    "Delivery runtime projection invalidation does not match its accepted runtime identity",
                ));
            }
        }
        RuntimeScopeKind::ProductSession => {
            let payload: ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent =
                serde_json::from_slice(&event.payload).map_err(|_| {
                    StorageError::invalid_input(
                        "ProductSession runtime projection invalidation is not canonical",
                    )
                })?;
            let canonical = serde_json::to_vec(&payload).map_err(|error| {
                StorageError::adapter(format!(
                    "failed to encode ProductSession runtime invalidation: {error}"
                ))
            })?;
            if canonical != event.payload
                || payload.product_session_id != context.product_session_id
                || payload.projection_revision != expected_revision
                || payload.last_projection_sequence != expected_sequence
                || payload.reload_queries
                    != (ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,)
                || payload.scope_kind
                    != ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventScopeKind::ProductSession
                || payload.type_value
                    != ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1
                || cursor.key().stream()
                    != &ProjectionEventStream::ProductSession(context.product_session_id.clone())
            {
                return Err(StorageError::invalid_input(
                    "ProductSession runtime projection invalidation does not match its accepted runtime identity",
                ));
            }
        }
    }
    Ok(())
}

/// Replay validates only the stored receipt and public invalidation bytes. It
/// deliberately does not load current state, an aggregate journal, or the
/// accepted runtime ledger.
fn validate_replay_invalidation(
    event: &winwincode_storage::OutboxEvent,
    receipt: &CommitReceipt,
) -> Result<(), StorageError> {
    let cursor = event.projection_cursor.as_ref().ok_or_else(|| {
        StorageError::invalid_input("runtime replay invalidation has no projection cursor")
    })?;
    if event.topic != RUNTIME_INVALIDATED_TOPIC
        || cursor.sequence() == 0
        || cursor.key().scope_key() != receipt.receipt_identity.scope_key()
        || cursor.event_id().map(|id| id.0.as_str()) != Some(event.event_id.as_str())
        || projection_event_id(
            RUNTIME_INVALIDATION_NAMESPACE,
            receipt.receipt_identity.scope_key(),
            &event.payload,
        )
        .0 != event.event_id
    {
        return Err(StorageError::invalid_input(
            "runtime replay invalidation cursor is foreign",
        ));
    }
    let product = serde_json::from_slice::<
        ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent,
    >(&event.payload);
    if let Ok(payload) = product {
        let canonical = serde_json::to_vec(&payload).map_err(|error| {
            StorageError::adapter(format!(
                "failed to encode ProductSession runtime replay invalidation: {error}"
            ))
        })?;
        let expected_sequence = i64::try_from(receipt.revision)
            .map_err(|_| StorageError::invalid_input("runtime replay sequence is out of range"))?;
        if canonical != event.payload
            || payload.projection_revision != Revision(expected_sequence)
            || payload.last_projection_sequence != expected_sequence
            || payload.scope_kind
                != ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventScopeKind::ProductSession
            || payload.type_value
                != ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1
            || payload.reload_queries
                != (ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,)
            || cursor.key().stream()
                != &ProjectionEventStream::ProductSession(payload.product_session_id)
        {
            return Err(StorageError::invalid_input(
                "ProductSession runtime replay invalidation is foreign",
            ));
        }
        return Ok(());
    }
    let payload: ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent =
        serde_json::from_slice(&event.payload).map_err(|_| {
            StorageError::invalid_input("runtime replay invalidation is not canonical")
        })?;
    let canonical = serde_json::to_vec(&payload).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode Delivery runtime replay invalidation: {error}"
        ))
    })?;
    let expected_sequence = i64::try_from(receipt.revision)
        .map_err(|_| StorageError::invalid_input("runtime replay sequence is out of range"))?;
    if canonical != event.payload
        || payload.projection_revision != Revision(expected_sequence)
        || payload.last_projection_sequence != expected_sequence
        || payload.scope_kind
            != ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind::DeliveryStage
        || payload.type_value
            != ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1
        || payload.reload_queries
            != (
                ControlPlaneWebSocketDeliveryGetReloadQuery::DeliveryGet,
                ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
            )
        || cursor.key().stream() != &ProjectionEventStream::Delivery(payload.delivery_id)
    {
        return Err(StorageError::invalid_input(
            "Delivery runtime replay invalidation is foreign",
        ));
    }
    Ok(())
}

pub(crate) fn runtime_stream_id_for_projection(
    scope_key: &ReceiptScopeKey,
    job_id: &ExecutionJobId,
) -> String {
    runtime_stream_id(scope_key, job_id)
}

pub(crate) fn runtime_ack_sequence_for_replay(
    storage: &dyn ProductStateStorage,
    scope_key: &ReceiptScopeKey,
    job_id: &ExecutionJobId,
) -> Result<ExecutionAckSequence, StorageError> {
    let stream_id = runtime_stream_id(scope_key, job_id);
    let sequence = match load_ledger(storage, &stream_id)? {
        Some(ledger) if ledger.execution_job_id == *job_id => ledger.highest_sequence,
        Some(_) => {
            return Err(StorageError::invalid_input(
                "runtime replay ledger belongs to another ExecutionJob",
            ));
        }
        None => 0,
    };
    i64::try_from(sequence)
        .map(ExecutionAckSequence)
        .map_err(|_| StorageError::invalid_input("runtime replay acknowledgement is out of range"))
}

fn runtime_stream_id(scope_key: &ReceiptScopeKey, job_id: &ExecutionJobId) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.runtime-ledger-stream.v1\0");
    digest.update((scope_key.as_bytes().len() as u64).to_be_bytes());
    digest.update(scope_key.as_bytes());
    digest.update((job_id.0.len() as u64).to_be_bytes());
    digest.update(job_id.0.as_bytes());
    format!("{RUNTIME_STREAM_PREFIX}{:x}", digest.finalize())
}

fn product_session_runtime_stream_id(product_session_id: &ProductSessionId) -> String {
    format!("product-session:{}", product_session_id.0)
}

pub(crate) fn product_session_runtime_stream_id_for_projection(
    product_session_id: &ProductSessionId,
) -> String {
    product_session_runtime_stream_id(product_session_id)
}

fn runtime_outbox_event_id(stream_id: &str, event: &ExecutionEventRecord) -> String {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.runtime-event-accepted.v1\0");
    digest.update(stream_id.as_bytes());
    digest.update(event.event_id.0.as_bytes());
    digest.update(event.sequence.0.to_be_bytes());
    format!("runtime-event:{:x}", digest.finalize())
}

fn runtime_projection_invalidated_event(
    receipt_identity: &ReceiptIdentity,
    context: &RuntimeContext,
    scope: &RepositoryScope,
    message: &RuntimeEventMessage,
    accepted_sequence: u64,
) -> Result<NewOutboxEvent, StorageError> {
    let revision = i64::try_from(accepted_sequence)
        .map(Revision)
        .map_err(|_| StorageError::invalid_input("runtime projection revision is out of range"))?;
    let accepted_sequence = i64::try_from(accepted_sequence)
        .map_err(|_| StorageError::invalid_input("runtime projection sequence is out of range"))?;
    let (payload, stream) = match context.scope_kind {
        RuntimeScopeKind::DeliveryStage => {
            let delivery_id = context.delivery_id.as_ref().ok_or_else(|| {
                StorageError::invalid_input("Delivery runtime invalidation has no Delivery id")
            })?;
            let stage_run_id = context.stage_run_id.as_ref().ok_or_else(|| {
                StorageError::invalid_input("Delivery runtime invalidation has no StageRun id")
            })?;
            let payload = ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEvent {
                delivery_id: delivery_id.clone(),
                last_projection_sequence: accepted_sequence,
                product_session_id: context.product_session_id.clone(),
                projection_revision: revision,
                reload_queries: (
                    ControlPlaneWebSocketDeliveryGetReloadQuery::DeliveryGet,
                    ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
                ),
                scope_kind:
                    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventScopeKind::DeliveryStage,
                session_identity: message.session_identity.clone(),
                stage_run_id: stage_run_id.clone(),
                type_value:
                    ControlPlaneWebSocketDeliveryStageRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1,
            };
            (
                serde_json::to_vec(&payload).map_err(|error| {
                    StorageError::adapter(format!(
                        "failed to encode runtime projection invalidation: {error}"
                    ))
                })?,
                ProjectionEventStream::Delivery(delivery_id.clone()),
            )
        }
        RuntimeScopeKind::ProductSession => {
            let payload = ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEvent {
                last_projection_sequence: accepted_sequence,
                product_session_id: context.product_session_id.clone(),
                projection_revision: revision,
                reload_queries: (
                    ControlPlaneWebSocketRuntimeProjectionGetReloadQuery::RuntimeProjectionGet,
                ),
                scope_kind:
                    ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventScopeKind::ProductSession,
                type_value:
                    ControlPlaneWebSocketProductSessionRuntimeProjectionInvalidatedEventTypeValue::RuntimeProjectionInvalidatedV1,
            };
            (
                serde_json::to_vec(&payload).map_err(|error| {
                    StorageError::adapter(format!(
                        "failed to encode runtime projection invalidation: {error}"
                    ))
                })?,
                ProjectionEventStream::ProductSession(context.product_session_id.clone()),
            )
        }
    };
    let event_id = projection_event_id(
        RUNTIME_INVALIDATION_NAMESPACE,
        receipt_identity.scope_key(),
        &payload,
    );
    NewOutboxEvent::public_projection(
        event_id,
        RUNTIME_INVALIDATED_TOPIC,
        payload,
        stream,
        crate::public_repository_scope(scope),
        message.sent_at.clone(),
        PublicEventSource::SessionExecutionWorker {
            worker_id: message.lease.worker_id.clone(),
            worker_session_id: message.worker_session_id.clone(),
            lease_id: message.lease.lease_id.clone(),
            codex_thread_id: message.codex_thread_id.clone(),
            session_identity: message.session_identity.clone(),
        },
    )
}

fn accepted_ack(
    message: &RuntimeEventMessage,
    sequence: u64,
    status: LeaseWriteStatus,
    replay_from_sequence: Option<u64>,
) -> RuntimeAckMessage {
    RuntimeAckMessage {
        ack_sequence: ExecutionAckSequence(i64::try_from(sequence).unwrap_or(i64::MAX)),
        error: None,
        kind: RuntimeAckMessageKind::RuntimeAck,
        lease: message.lease.clone(),
        message_id: ack_message_id(&message.message_id),
        replay_from_sequence: replay_from_sequence
            .map(|value| ExecutionSequence(i64::try_from(value).unwrap_or(i64::MAX))),
        schema_version: SchemaVersion::WinwincodeV1,
        session_identity: message.session_identity.clone(),
        sent_at: message.sent_at.clone(),
        status,
        worker_session_id: message.worker_session_id.clone(),
    }
}

fn gap_ack(message: &RuntimeEventMessage, sequence: u64, next: u64) -> RuntimeAckMessage {
    let mut ack = accepted_ack(message, sequence, LeaseWriteStatus::Gap, Some(next));
    ack.error = Some(ExecutionPortError {
        code: ExecutionPortErrorCode::SequenceGap,
        message: "runtime event sequence has a gap".to_owned(),
        retryable: true,
    });
    ack
}

fn rejection_ack(
    message: &RuntimeEventMessage,
    sequence: u64,
    rejection: Rejection,
) -> RuntimeAckMessage {
    accepted_ack_with_error(
        message,
        sequence,
        rejection.status(),
        None,
        ExecutionPortError {
            code: rejection.code(),
            message: rejection.message().to_owned(),
            retryable: rejection.retryable(),
        },
    )
}

fn accepted_ack_with_error(
    message: &RuntimeEventMessage,
    sequence: u64,
    status: LeaseWriteStatus,
    replay_from_sequence: Option<u64>,
    error: ExecutionPortError,
) -> RuntimeAckMessage {
    let mut ack = accepted_ack(message, sequence, status, replay_from_sequence);
    ack.error = Some(error);
    ack
}

fn ack_message_id(message_id: &ExecutionMessageId) -> ExecutionMessageId {
    let mut digest = Sha256::new();
    digest.update(b"winwincode.runtime-ack-message.v1\0");
    digest.update(message_id.0.as_bytes());
    let bytes = digest.finalize();
    let mut value = [0_u8; 16];
    value.copy_from_slice(&bytes[..16]);
    let mut number = u128::from_be_bytes(value);
    let mut suffix = [b'0'; 26];
    for index in (0..suffix.len()).rev() {
        suffix[index] = ACK_ID_ALPHABET[(number & 31) as usize];
        number >>= 5;
    }
    let suffix = std::str::from_utf8(&suffix).unwrap_or("00000000000000000000000000");
    ExecutionMessageId(format!("xmsg_{suffix}"))
}

fn decimal_token(token: &FencingToken) -> u128 {
    token
        .0
        .bytes()
        .try_fold(0_u128, |value, byte| {
            value
                .checked_mul(10)
                .and_then(|value| value.checked_add(u128::from(byte - b'0')))
        })
        .unwrap_or(u128::MAX)
}
