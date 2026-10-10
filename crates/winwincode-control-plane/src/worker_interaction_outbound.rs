// SPDX-License-Identifier: Apache-2.0

//! Durable Control Plane-to-Worker interaction and lease-renewal delivery.
//!
//! The adapter maps canonical interaction and lease-renewal messages to the
//! durable Worker outbound queue. Raw input values and
//! approval reasons remain inside the restricted queue and the explicit
//! Worker connection boundary.

use std::{fmt, path::Path};

use winwincode_domain::{ExecutionMessageId, Instant, Sha256Digest, WorkerId, WorkerInstanceId};
use winwincode_execution_port::{
    generated::{ExecutionLeaseStamp, ExecutionPortMessage, LeaseRenewMessage},
    transport::{
        AdapterError, EndpointSide, ExecutionPortCore, FrameDirection, LocalWorkerAdapter,
        RemoteTransportAdapter, TypedFrame,
    },
};
use winwincode_storage::{
    ExecutionLeaseRenewal, LeaseWriteStatus, ProductStateStorage, SqliteStorage,
    WorkerOutboundAcknowledgement, WorkerOutboundAuthority, WorkerOutboundClaim,
    WorkerOutboundConfirmationProgress, WorkerOutboundEnqueueRequest, WorkerOutboundPageCursor,
    WorkerOutboundQueueConfig, WorkerOutboundQueueError, WorkerOutboundQueueErrorCode,
    WorkerSlotAuthority,
};

use crate::{
    WorkerInteractionDeliveryError, WorkerInteractionDeliveryErrorKind,
    WorkerInteractionOutboundPort,
};

/// Stable Worker connection failure categories. None contains interaction data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerInteractionConnectionErrorKind {
    InvalidInput,
    AuthorityRejected,
    Conflict,
    CorruptFrame,
    Unavailable,
}

/// Bounded connection-side queue failure with no input value or decision reason.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerInteractionConnectionError {
    kind: WorkerInteractionConnectionErrorKind,
    message: &'static str,
}

impl WorkerInteractionConnectionError {
    const fn new(kind: WorkerInteractionConnectionErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    const fn invalid() -> Self {
        Self::new(
            WorkerInteractionConnectionErrorKind::InvalidInput,
            "Worker interaction connection request is invalid",
        )
    }

    const fn corrupt() -> Self {
        Self::new(
            WorkerInteractionConnectionErrorKind::CorruptFrame,
            "Worker interaction queue contains an invalid typed frame",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> WorkerInteractionConnectionErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerInteractionConnectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for WorkerInteractionConnectionError {}

/// Opaque stable cursor bound by storage to one exact Worker authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerInteractionPageCursor(WorkerOutboundPageCursor);

/// One validated canonical frame for either an in-process or remote Worker.
#[derive(Clone, PartialEq)]
pub struct WorkerInteractionClaim {
    message_id: ExecutionMessageId,
    payload_digest: Sha256Digest,
    encoded_frame: Vec<u8>,
    typed_frame: TypedFrame,
    delivery_attempt: u64,
    replayed: bool,
}

impl WorkerInteractionClaim {
    #[must_use]
    pub const fn message_id(&self) -> &ExecutionMessageId {
        &self.message_id
    }

    #[must_use]
    pub const fn payload_digest(&self) -> &Sha256Digest {
        &self.payload_digest
    }

    /// Returns the canonical frame used by an in-process Worker adapter.
    #[must_use]
    pub const fn typed_frame(&self) -> &TypedFrame {
        &self.typed_frame
    }

    /// Returns the exact same canonical frame encoded for a remote Worker.
    #[must_use]
    pub fn encoded_frame(&self) -> &[u8] {
        &self.encoded_frame
    }

    #[must_use]
    pub const fn delivery_attempt(&self) -> u64 {
        self.delivery_attempt
    }

    #[must_use]
    pub const fn replayed(&self) -> bool {
        self.replayed
    }

    /// Delivers this already-validated frame to an in-process Worker through
    /// the canonical local adapter.
    ///
    /// # Errors
    ///
    /// Returns the adapter or Worker core error without logging the frame.
    pub fn deliver_local<Core: ExecutionPortCore + ?Sized>(
        &self,
        core: &mut Core,
    ) -> Result<Core::Output, AdapterError<Core::Error>> {
        LocalWorkerAdapter::new(core, EndpointSide::Worker).accept(&self.typed_frame)
    }
}

impl fmt::Debug for WorkerInteractionClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerInteractionClaim")
            .field("message_id", &self.message_id)
            .field("payload_digest", &self.payload_digest)
            .field("encoded_frame", &"<redacted>")
            .field("typed_frame", &"<redacted>")
            .field("delivery_attempt", &self.delivery_attempt)
            .field("replayed", &self.replayed)
            .finish()
    }
}

/// One stable delivery page. Newly enqueued frames do not enter its page cut.
#[derive(Debug, PartialEq)]
pub struct WorkerInteractionClaimPage {
    pub claims: Vec<WorkerInteractionClaim>,
    pub next_cursor: Option<WorkerInteractionPageCursor>,
}

/// Production durable interaction delivery and atomic lease-renewal adapter.
pub struct DurableWorkerInteractionOutbound {
    storage: SqliteStorage,
    config: WorkerOutboundQueueConfig,
}

impl DurableWorkerInteractionOutbound {
    /// Creates an adapter around its own `SQLite` connection and prepares the
    /// restricted queue before the adapter can be injected.
    ///
    /// # Errors
    ///
    /// Returns a bounded error for invalid limits, permissions, or schema setup.
    pub fn new(
        mut storage: SqliteStorage,
        config: WorkerOutboundQueueConfig,
    ) -> Result<Self, WorkerInteractionConnectionError> {
        storage
            .worker_outbound_queue(config)
            .map_err(|error| map_connection_error(&error))?;
        Ok(Self { storage, config })
    }

    /// Returns the canonical database path so the composition root can prove
    /// this adapter and the Control Plane state storage use the same database.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        self.storage.database_path()
    }

    /// Checkpoints the shared database and closes both adapter connections.
    ///
    /// # Errors
    ///
    /// Returns a bounded unavailable error if checkpointing or close fails.
    pub fn close(self) -> Result<(), WorkerInteractionConnectionError> {
        Box::new(self.storage).close().map_err(|_| {
            WorkerInteractionConnectionError::new(
                WorkerInteractionConnectionErrorKind::Unavailable,
                "Worker interaction durable queue close failed",
            )
        })
    }

    /// Atomically renews an exact running Worker slot and retains the canonical
    /// renewal frame. Returns its extended authority for the connection cache.
    ///
    /// # Errors
    ///
    /// Rejects invalid protocol frames, foreign/stale slots, conflicting renewal
    /// replays, and queue/storage failures. Failed enqueue rolls back renewal.
    pub fn renew_lease(
        &mut self,
        slot: &WorkerSlotAuthority,
        message: &LeaseRenewMessage,
    ) -> Result<WorkerOutboundAuthority, WorkerInteractionConnectionError> {
        let route = renewal_route(message, slot)?;
        let frame = TypedFrame::new(
            FrameDirection::ControlPlaneToWorker,
            ExecutionPortMessage::LeaseRenewMessage(message.clone()),
        )
        .map_err(|_| WorkerInteractionConnectionError::invalid())?;
        let bytes = RemoteTransportAdapter::<FrameCodec>::encode(&frame)
            .map_err(|_| WorkerInteractionConnectionError::invalid())?;
        let request = WorkerOutboundEnqueueRequest::new(
            route.authority.clone(),
            route.message_id,
            route.sent_at,
            bytes,
        )
        .map_err(|error| map_connection_error(&error))?;
        let lease = &message.lease;
        let renewal = ExecutionLeaseRenewal {
            expires_at: lease.expires_at.clone(),
            prior_expires_at: message.prior_expires_at.clone(),
            fencing_token: lease.fencing_token.clone(),
            job_id: lease.job_id.clone(),
            lease_id: lease.lease_id.clone(),
            message_id: message.message_id.clone(),
            request_id: message.request_id.clone(),
            sent_at: message.sent_at.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            attempt: route.authority.slot.attempt,
        };
        let receipt = self
            .storage
            .worker_outbound_queue(self.config)
            .map_err(|error| map_connection_error(&error))?
            .renew_lease_and_enqueue(&renewal, &request)
            .map_err(|error| map_connection_error(&error))?;
        if !matches!(
            receipt.status,
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
        ) {
            return Err(WorkerInteractionConnectionError::new(
                WorkerInteractionConnectionErrorKind::AuthorityRejected,
                "Worker lease renewal was rejected",
            ));
        }
        Ok(route.authority)
    }

    /// Claims and validates a stable page for one healthy reconnected Worker.
    /// Both previously claimed and pending frames are returned until ack.
    ///
    /// # Errors
    ///
    /// Rejects stale authority/cursors and fails closed on any invalid frame.
    pub fn claim_page(
        &mut self,
        authority: &WorkerOutboundAuthority,
        observed_at: &Instant,
        cursor: Option<&WorkerInteractionPageCursor>,
        page_size: usize,
    ) -> Result<WorkerInteractionClaimPage, WorkerInteractionConnectionError> {
        let page = self
            .storage
            .worker_outbound_queue(self.config)
            .map_err(|error| map_connection_error(&error))?
            .claim_page(
                authority,
                observed_at,
                cursor.map(|value| &value.0),
                page_size,
            )
            .map_err(|error| map_connection_error(&error))?;
        let claims = page
            .claims
            .iter()
            .map(|claim| validate_claim(authority, claim))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(WorkerInteractionClaimPage {
            claims,
            next_cursor: page.next_cursor.map(WorkerInteractionPageCursor),
        })
    }

    /// Claims the current queue cut and orders renewals before interactions,
    /// including renewals that occur on later pages. Renewal order is preserved.
    /// The queue's configured retained-byte limit bounds this batch in memory.
    ///
    /// # Errors
    ///
    /// Rejects invalid authority, corrupt frames, and storage failures before
    /// returning any message for delivery.
    pub fn claim_pending(
        &mut self,
        authority: &WorkerOutboundAuthority,
        observed_at: &Instant,
    ) -> Result<Vec<WorkerInteractionClaim>, WorkerInteractionConnectionError> {
        let mut claims = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.claim_page(
                authority,
                observed_at,
                cursor.as_ref(),
                self.config.max_claim_page_size,
            )?;
            claims.extend(page.claims);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        claims.sort_by_key(|claim| {
            !matches!(
                claim.typed_frame().message(),
                ExecutionPortMessage::LeaseRenewMessage(_)
            )
        });
        Ok(claims)
    }

    /// Reliably accepts an authenticated remote receipt before raw-frame cleanup.
    ///
    /// # Errors
    ///
    /// Rejects foreign or unclaimed interaction receipts and storage failures.
    pub fn confirm_remote(
        &mut self,
        worker: &WorkerId,
        instance: &WorkerInstanceId,
        message: &ExecutionMessageId,
        now: &Instant,
    ) -> Result<bool, WorkerInteractionConnectionError> {
        self.storage
            .worker_outbound_queue(self.config)
            .map_err(|error| map_connection_error(&error))?
            .confirm_remote(worker, instance, message, now)
            .map_err(|error| map_connection_error(&error))
    }

    /// Retries proven cleanup independently of repeated transport ACKs.
    ///
    /// # Errors
    ///
    /// Rejects malformed durable proofs or failure to load the cleanup batch.
    pub fn retry_confirmed(
        &mut self,
        worker: &WorkerId,
        instance: &WorkerInstanceId,
    ) -> Result<WorkerOutboundConfirmationProgress, WorkerInteractionConnectionError> {
        self.storage
            .worker_outbound_queue(self.config)
            .map_err(|error| map_connection_error(&error))?
            .retry_confirmed(worker, instance)
            .map_err(|error| map_connection_error(&error))
    }

    /// Acknowledges a claimed message and securely removes its raw frame.
    ///
    /// # Errors
    ///
    /// Rejects stale/foreign authority and unclaimed or conflicting messages.
    pub fn acknowledge(
        &mut self,
        authority: &WorkerOutboundAuthority,
        message_id: &ExecutionMessageId,
        acknowledged_at: &Instant,
    ) -> Result<WorkerOutboundAcknowledgement, WorkerInteractionConnectionError> {
        self.storage
            .worker_outbound_queue(self.config)
            .map_err(|error| map_connection_error(&error))?
            .acknowledge(authority, message_id, acknowledged_at)
            .map_err(|error| map_connection_error(&error))
    }

    /// Clears all raw interaction frames after the exact slot is terminal.
    ///
    /// # Errors
    ///
    /// Rejects a non-terminal or foreign authority and storage failures.
    pub fn settle_terminal(
        &mut self,
        authority: &WorkerOutboundAuthority,
        settled_at: &Instant,
    ) -> Result<usize, WorkerInteractionConnectionError> {
        self.storage
            .worker_outbound_queue(self.config)
            .map_err(|error| map_connection_error(&error))?
            .settle_terminal(authority, settled_at)
            .map_err(|error| map_connection_error(&error))
    }
}

impl fmt::Debug for DurableWorkerInteractionOutbound {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DurableWorkerInteractionOutbound")
            .field("database_path", &"<redacted>")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl WorkerInteractionOutboundPort for DurableWorkerInteractionOutbound {
    fn deliver(
        &mut self,
        message: &ExecutionPortMessage,
    ) -> Result<(), WorkerInteractionDeliveryError> {
        let route = interaction_route(message).map_err(map_delivery_route_error)?;
        let typed_frame = TypedFrame::new(FrameDirection::ControlPlaneToWorker, message.clone())
            .map_err(|_| rejected_delivery())?;
        let encoded_frame = RemoteTransportAdapter::<FrameCodec>::encode(&typed_frame)
            .map_err(|_| rejected_delivery())?;
        let request = WorkerOutboundEnqueueRequest::new(
            route.authority,
            route.message_id,
            route.sent_at,
            encoded_frame,
        )
        .map_err(|error| map_delivery_queue_error(&error))?;
        self.storage
            .worker_outbound_queue(self.config)
            .map_err(|error| map_delivery_queue_error(&error))?
            .enqueue(&request)
            .map_err(|error| map_delivery_queue_error(&error))?;
        Ok(())
    }
}

struct InteractionRoute {
    authority: WorkerOutboundAuthority,
    message_id: ExecutionMessageId,
    sent_at: Instant,
}

fn interaction_route(
    message: &ExecutionPortMessage,
) -> Result<InteractionRoute, WorkerInteractionConnectionError> {
    match message {
        ExecutionPortMessage::InputResponseMessage(message) => route_from_fields(
            &message.lease,
            &message.session_identity,
            &message.worker_session_id,
            &message.message_id,
            &message.sent_at,
        ),
        ExecutionPortMessage::ApprovalDecisionMessage(message) => route_from_fields(
            &message.lease,
            &message.session_identity,
            &message.worker_session_id,
            &message.message_id,
            &message.sent_at,
        ),
        _ => Err(WorkerInteractionConnectionError::invalid()),
    }
}

fn route_from_fields(
    lease: &ExecutionLeaseStamp,
    session_identity: &winwincode_domain::SessionIdentity,
    worker_session_id: &winwincode_domain::WorkerSessionId,
    message_id: &ExecutionMessageId,
    sent_at: &Instant,
) -> Result<InteractionRoute, WorkerInteractionConnectionError> {
    if session_identity.worker_session_id != *worker_session_id {
        return Err(WorkerInteractionConnectionError::invalid());
    }
    let attempt = u64::try_from(lease.attempt)
        .ok()
        .filter(|attempt| *attempt > 0)
        .ok_or_else(WorkerInteractionConnectionError::invalid)?;
    Ok(InteractionRoute {
        authority: WorkerOutboundAuthority {
            slot: WorkerSlotAuthority {
                worker_id: lease.worker_id.clone(),
                worker_instance_id: lease.worker_instance_id.clone(),
                worker_session_id: worker_session_id.clone(),
                codex_thread_id: session_identity.codex_thread_id.clone(),
                job_id: lease.job_id.clone(),
                lease_id: lease.lease_id.clone(),
                attempt,
                fencing_token: lease.fencing_token.clone(),
            },
            lease_issued_at: lease.issued_at.clone(),
            lease_expires_at: lease.expires_at.clone(),
        },
        message_id: message_id.clone(),
        sent_at: sent_at.clone(),
    })
}

fn renewal_route(
    message: &LeaseRenewMessage,
    slot: &WorkerSlotAuthority,
) -> Result<InteractionRoute, WorkerInteractionConnectionError> {
    let lease = &message.lease;
    if u64::try_from(lease.attempt).ok() != Some(slot.attempt)
        || lease.job_id != slot.job_id
        || lease.lease_id != slot.lease_id
        || lease.worker_id != slot.worker_id
        || lease.worker_instance_id != slot.worker_instance_id
        || lease.fencing_token != slot.fencing_token
    {
        return Err(WorkerInteractionConnectionError::invalid());
    }
    Ok(InteractionRoute {
        authority: WorkerOutboundAuthority {
            slot: slot.clone(),
            lease_issued_at: lease.issued_at.clone(),
            lease_expires_at: lease.expires_at.clone(),
        },
        message_id: message.message_id.clone(),
        sent_at: message.sent_at.clone(),
    })
}

fn validate_claim(
    expected_authority: &WorkerOutboundAuthority,
    claim: &WorkerOutboundClaim,
) -> Result<WorkerInteractionClaim, WorkerInteractionConnectionError> {
    let typed_frame = RemoteTransportAdapter::<FrameCodec>::decode(claim.frame_bytes())
        .map_err(|_| WorkerInteractionConnectionError::corrupt())?;
    if typed_frame.direction() != FrameDirection::ControlPlaneToWorker {
        return Err(WorkerInteractionConnectionError::corrupt());
    }
    let route = match typed_frame.message() {
        ExecutionPortMessage::LeaseRenewMessage(message) => {
            renewal_route(message, &claim.authority().slot)
        }
        message => interaction_route(message),
    }
    .map_err(|_| WorkerInteractionConnectionError::corrupt())?;
    let mut current_authority = claim.authority().clone();
    current_authority
        .lease_expires_at
        .clone_from(&expected_authority.lease_expires_at);
    if current_authority != *expected_authority
        || claim.authority().lease_expires_at.0 > expected_authority.lease_expires_at.0
        || route.authority != *claim.authority()
        || route.message_id != *claim.message_id()
    {
        return Err(WorkerInteractionConnectionError::corrupt());
    }
    Ok(WorkerInteractionClaim {
        message_id: claim.message_id().clone(),
        payload_digest: claim.payload_digest().clone(),
        encoded_frame: claim.frame_bytes().to_vec(),
        typed_frame,
        delivery_attempt: claim.delivery_attempt(),
        replayed: claim.replayed(),
    })
}

fn map_delivery_route_error(_: WorkerInteractionConnectionError) -> WorkerInteractionDeliveryError {
    rejected_delivery()
}

fn rejected_delivery() -> WorkerInteractionDeliveryError {
    WorkerInteractionDeliveryError::new(
        WorkerInteractionDeliveryErrorKind::Rejected,
        "Worker interaction message is not routable",
    )
}

fn map_delivery_queue_error(error: &WorkerOutboundQueueError) -> WorkerInteractionDeliveryError {
    let kind = match error.code() {
        WorkerOutboundQueueErrorCode::CapacityExceeded | WorkerOutboundQueueErrorCode::Storage => {
            WorkerInteractionDeliveryErrorKind::Unavailable
        }
        WorkerOutboundQueueErrorCode::InvalidInput
        | WorkerOutboundQueueErrorCode::AuthorityMismatch
        | WorkerOutboundQueueErrorCode::AuthorityExpired
        | WorkerOutboundQueueErrorCode::MessageConflict
        | WorkerOutboundQueueErrorCode::StateConflict => {
            WorkerInteractionDeliveryErrorKind::Rejected
        }
    };
    WorkerInteractionDeliveryError::new(kind, "Worker interaction delivery was not accepted")
}

fn map_connection_error(error: &WorkerOutboundQueueError) -> WorkerInteractionConnectionError {
    match error.code() {
        WorkerOutboundQueueErrorCode::InvalidInput => WorkerInteractionConnectionError::invalid(),
        WorkerOutboundQueueErrorCode::AuthorityMismatch
        | WorkerOutboundQueueErrorCode::AuthorityExpired => WorkerInteractionConnectionError::new(
            WorkerInteractionConnectionErrorKind::AuthorityRejected,
            "Worker interaction authority was rejected",
        ),
        WorkerOutboundQueueErrorCode::MessageConflict
        | WorkerOutboundQueueErrorCode::StateConflict => WorkerInteractionConnectionError::new(
            WorkerInteractionConnectionErrorKind::Conflict,
            "Worker interaction durable state conflicts with this request",
        ),
        WorkerOutboundQueueErrorCode::CapacityExceeded | WorkerOutboundQueueErrorCode::Storage => {
            WorkerInteractionConnectionError::new(
                WorkerInteractionConnectionErrorKind::Unavailable,
                "Worker interaction durable queue is unavailable",
            )
        }
    }
}

struct FrameCodec;

impl ExecutionPortCore for FrameCodec {
    type Output = ();
    type Error = std::convert::Infallible;

    fn accept(&mut self, _message: &ExecutionPortMessage) -> Result<Self::Output, Self::Error> {
        Ok(())
    }
}
