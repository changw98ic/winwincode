// SPDX-License-Identifier: Apache-2.0

//! Restricted durable queue for Control Plane-to-Worker interaction frames.
//!
//! Raw interaction frames live only in this internal table and are deleted
//! atomically after acknowledgement or terminal settlement. Public state,
//! outbox events, audit records, errors, and `Debug` output contain only
//! authority, identifiers, digests, states, and counts.

use std::{collections::BTreeMap, fmt, fs};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ExecutionMessageId, Instant, Sha256Digest, WorkerId, WorkerInstanceId, WorkerSessionId,
};

use crate::execution_registry::{
    accepted_lease_history, load_lease_in_transaction, renew_execution_lease_in_transaction,
};
use crate::worker_session_slots::{
    load_slot_in_transaction, require_retained_slot_authority, require_terminal_slot_authority,
};
use crate::{
    ExecutionLeaseReceipt, ExecutionLeaseRenewal, LeaseWriteStatus, SqliteStorage,
    WorkerSlotAuthority, WorkerSlotError, WorkerSlotErrorCode,
};

const WORKER_OUTBOUND_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS internal_worker_outbound_messages (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT UNIQUE NOT NULL,
    authority_digest TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    worker_session_id TEXT NOT NULL,
    job_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0 AND attempt <= 1000),
    fencing_token TEXT NOT NULL,
    sent_at TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    payload BLOB NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'claimed')),
    delivery_attempts INTEGER NOT NULL CHECK (delivery_attempts >= 0),
    claimed_at TEXT,
    CHECK (
        (state = 'pending' AND delivery_attempts = 0 AND claimed_at IS NULL)
        OR (state = 'claimed' AND delivery_attempts > 0 AND claimed_at IS NOT NULL)
    )
);
CREATE TABLE IF NOT EXISTS internal_worker_outbound_settlements (
    message_id TEXT PRIMARY KEY NOT NULL,
    authority_digest TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    settlement TEXT NOT NULL CHECK (settlement IN ('acknowledged', 'terminal')),
    settled_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS internal_worker_outbound_confirmations (
    message_id TEXT PRIMARY KEY NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    authority_digest TEXT NOT NULL,
    authority_json BLOB NOT NULL,
    payload_digest TEXT NOT NULL,
    confirmed_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS internal_worker_outbound_confirmation_route
    ON internal_worker_outbound_confirmations (worker_id, worker_instance_id, message_id);
CREATE INDEX IF NOT EXISTS internal_worker_outbound_route
    ON internal_worker_outbound_messages (
        authority_digest, sequence, message_id
    );
";

const MAX_QUEUE_CAPACITY: usize = 100_000;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 200;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Bounded queue settings owned by the host composition root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerOutboundQueueConfig {
    pub max_frame_bytes: usize,
    pub max_pending_messages_per_authority: usize,
    pub max_retained_bytes: usize,
    pub max_claim_page_size: usize,
}

impl Default for WorkerOutboundQueueConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1024 * 1024,
            max_pending_messages_per_authority: 1_024,
            max_retained_bytes: 64 * 1024 * 1024,
            max_claim_page_size: 100,
        }
    }
}

/// Exact current Worker slot and complete lease time authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerOutboundAuthority {
    pub slot: WorkerSlotAuthority,
    pub lease_issued_at: Instant,
    pub lease_expires_at: Instant,
}

/// Durable outbound lifecycle while raw bytes remain retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerOutboundMessageState {
    Pending,
    Claimed,
}

impl WorkerOutboundMessageState {
    fn parse(value: &str) -> Result<Self, WorkerOutboundQueueError> {
        match value {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            _ => Err(WorkerOutboundQueueError::storage()),
        }
    }
}

/// Secret-free terminal disposition retained after raw frame cleanup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerOutboundSettlement {
    Acknowledged,
    Terminal,
}

impl WorkerOutboundSettlement {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Acknowledged => "acknowledged",
            Self::Terminal => "terminal",
        }
    }

    fn parse(value: &str) -> Result<Self, WorkerOutboundQueueError> {
        match value {
            "acknowledged" => Ok(Self::Acknowledged),
            "terminal" => Ok(Self::Terminal),
            _ => Err(WorkerOutboundQueueError::storage()),
        }
    }
}

/// New internal frame. `Debug` deliberately omits the raw bytes.
pub struct WorkerOutboundEnqueueRequest {
    authority: WorkerOutboundAuthority,
    message_id: ExecutionMessageId,
    sent_at: Instant,
    payload_digest: Sha256Digest,
    frame_bytes: Vec<u8>,
}

impl WorkerOutboundEnqueueRequest {
    /// Seals one raw canonical typed frame to its digest.
    ///
    /// # Errors
    ///
    /// Rejects an empty frame or malformed authority, identity, or time.
    pub fn new(
        authority: WorkerOutboundAuthority,
        message_id: ExecutionMessageId,
        sent_at: Instant,
        frame_bytes: Vec<u8>,
    ) -> Result<Self, WorkerOutboundQueueError> {
        validate_authority(&authority)?;
        validate_message_id(&message_id)?;
        validate_instant(&sent_at)?;
        if frame_bytes.is_empty() {
            return Err(WorkerOutboundQueueError::invalid());
        }
        let payload_digest = digest_bytes(&frame_bytes);
        Ok(Self {
            authority,
            message_id,
            sent_at,
            payload_digest,
            frame_bytes,
        })
    }

    #[must_use]
    pub const fn authority(&self) -> &WorkerOutboundAuthority {
        &self.authority
    }

    #[must_use]
    pub const fn message_id(&self) -> &ExecutionMessageId {
        &self.message_id
    }

    #[must_use]
    pub const fn payload_digest(&self) -> &Sha256Digest {
        &self.payload_digest
    }
}

impl fmt::Debug for WorkerOutboundEnqueueRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerOutboundEnqueueRequest")
            .field("authority", &self.authority)
            .field("message_id", &self.message_id)
            .field("sent_at", &self.sent_at)
            .field("payload_digest", &self.payload_digest)
            .field("frame_bytes", &"<redacted>")
            .finish()
    }
}

/// Secret-free durable enqueue result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerOutboundEnqueueReceipt {
    pub message_id: ExecutionMessageId,
    pub payload_digest: Sha256Digest,
    pub state: Option<WorkerOutboundMessageState>,
    pub settlement: Option<WorkerOutboundSettlement>,
    pub replayed: bool,
}

/// One claimed frame. `Debug` deliberately omits the raw bytes.
#[derive(Clone, Eq, PartialEq)]
pub struct WorkerOutboundClaim {
    authority: WorkerOutboundAuthority,
    message_id: ExecutionMessageId,
    payload_digest: Sha256Digest,
    frame_bytes: Vec<u8>,
    delivery_attempt: u64,
    replayed: bool,
}

impl WorkerOutboundClaim {
    /// The exact accepted lease authority under which this frame was retained.
    #[must_use]
    pub const fn authority(&self) -> &WorkerOutboundAuthority {
        &self.authority
    }

    #[must_use]
    pub const fn message_id(&self) -> &ExecutionMessageId {
        &self.message_id
    }

    #[must_use]
    pub const fn payload_digest(&self) -> &Sha256Digest {
        &self.payload_digest
    }

    #[must_use]
    pub fn frame_bytes(&self) -> &[u8] {
        &self.frame_bytes
    }

    #[must_use]
    pub const fn delivery_attempt(&self) -> u64 {
        self.delivery_attempt
    }

    #[must_use]
    pub const fn replayed(&self) -> bool {
        self.replayed
    }
}

impl fmt::Debug for WorkerOutboundClaim {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkerOutboundClaim")
            .field("authority", &self.authority)
            .field("message_id", &self.message_id)
            .field("payload_digest", &self.payload_digest)
            .field("frame_bytes", &"<redacted>")
            .field("delivery_attempt", &self.delivery_attempt)
            .field("replayed", &self.replayed)
            .finish()
    }
}

/// Opaque stable page cursor bound to one exact authority and page size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerOutboundPageCursor {
    authority_digest: String,
    after_sequence: u64,
    snapshot_sequence: u64,
    page_size: usize,
}

/// One stable claim page. Newly enqueued frames never enter an existing page cut.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerOutboundClaimPage {
    pub claims: Vec<WorkerOutboundClaim>,
    pub next_cursor: Option<WorkerOutboundPageCursor>,
}

/// Secret-free acknowledgement or cleanup result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerOutboundAcknowledgement {
    pub message_id: ExecutionMessageId,
    pub settlement: WorkerOutboundSettlement,
    pub replayed: bool,
}

/// Completed cleanups plus a retained, independently retryable cleanup failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerOutboundConfirmationProgress {
    pub acknowledgements: Vec<WorkerOutboundAcknowledgement>,
    pub retry_error: Option<WorkerOutboundQueueErrorCode>,
}

/// Stable queue failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerOutboundQueueErrorCode {
    InvalidInput,
    AuthorityMismatch,
    AuthorityExpired,
    CapacityExceeded,
    MessageConflict,
    StateConflict,
    Storage,
}

/// Bounded queue failure with no raw frame or interaction content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerOutboundQueueError {
    code: WorkerOutboundQueueErrorCode,
    message: &'static str,
}

impl WorkerOutboundQueueError {
    const fn new(code: WorkerOutboundQueueErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    const fn invalid() -> Self {
        Self::new(
            WorkerOutboundQueueErrorCode::InvalidInput,
            "Worker outbound queue request is invalid",
        )
    }

    const fn authority() -> Self {
        Self::new(
            WorkerOutboundQueueErrorCode::AuthorityMismatch,
            "Worker outbound authority is not current",
        )
    }

    const fn expired() -> Self {
        Self::new(
            WorkerOutboundQueueErrorCode::AuthorityExpired,
            "Worker outbound authority has expired",
        )
    }

    const fn capacity() -> Self {
        Self::new(
            WorkerOutboundQueueErrorCode::CapacityExceeded,
            "Worker outbound queue capacity is exhausted",
        )
    }

    const fn conflict() -> Self {
        Self::new(
            WorkerOutboundQueueErrorCode::MessageConflict,
            "Worker outbound messageId conflicts with durable input",
        )
    }

    const fn state() -> Self {
        Self::new(
            WorkerOutboundQueueErrorCode::StateConflict,
            "Worker outbound message state rejects this operation",
        )
    }

    const fn storage() -> Self {
        Self::new(
            WorkerOutboundQueueErrorCode::Storage,
            "Worker outbound queue storage operation failed",
        )
    }

    #[must_use]
    pub const fn code(&self) -> WorkerOutboundQueueErrorCode {
        self.code
    }
}

impl fmt::Display for WorkerOutboundQueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for WorkerOutboundQueueError {}

impl From<WorkerSlotError> for WorkerOutboundQueueError {
    fn from(error: WorkerSlotError) -> Self {
        match error.code() {
            WorkerSlotErrorCode::LeaseExpired => Self::expired(),
            WorkerSlotErrorCode::Adapter => Self::storage(),
            WorkerSlotErrorCode::RequestConflict => Self::conflict(),
            WorkerSlotErrorCode::StateConflict => Self::state(),
            WorkerSlotErrorCode::InvalidInput
            | WorkerSlotErrorCode::RevisionConflict
            | WorkerSlotErrorCode::CursorConflict
            | WorkerSlotErrorCode::WorkerNotCurrent
            | WorkerSlotErrorCode::WorkerNotHealthy
            | WorkerSlotErrorCode::LeaseMismatch
            | WorkerSlotErrorCode::AdmissionNotRunning
            | WorkerSlotErrorCode::CapacityExhausted
            | WorkerSlotErrorCode::ResourceExhausted => Self::authority(),
        }
    }
}

/// Borrowed durable internal outbound queue.
pub struct WorkerOutboundQueue<'storage> {
    storage: &'storage mut SqliteStorage,
    config: WorkerOutboundQueueConfig,
}

impl SqliteStorage {
    /// Opens the restricted outbound queue over this exact database.
    ///
    /// # Errors
    ///
    /// Rejects invalid bounds or a schema/permission preparation failure.
    pub fn worker_outbound_queue(
        &mut self,
        config: WorkerOutboundQueueConfig,
    ) -> Result<WorkerOutboundQueue<'_>, WorkerOutboundQueueError> {
        WorkerOutboundQueue::new(self, config)
    }
}

impl<'storage> WorkerOutboundQueue<'storage> {
    /// Prepares the restricted internal tables and directory permissions.
    ///
    /// # Errors
    ///
    /// Rejects invalid bounds or a schema/permission preparation failure.
    pub fn new(
        storage: &'storage mut SqliteStorage,
        config: WorkerOutboundQueueConfig,
    ) -> Result<Self, WorkerOutboundQueueError> {
        validate_config(config)?;
        restrict_database_permissions(storage)?;
        storage
            .connection()
            .map_err(|_| WorkerOutboundQueueError::storage())?
            .execute_batch(&format!(
                "PRAGMA secure_delete = ON;{WORKER_OUTBOUND_SCHEMA}"
            ))
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        Ok(Self { storage, config })
    }

    /// Durably accepts a frame after current slot and lease validation.
    /// Worker liveness is intentionally not required, so disconnected Workers
    /// retain messages for reconnect delivery.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, capacity exhaustion, changed-body duplicates,
    /// oversized frames, and storage failures.
    pub fn enqueue(
        &mut self,
        request: &WorkerOutboundEnqueueRequest,
    ) -> Result<WorkerOutboundEnqueueReceipt, WorkerOutboundQueueError> {
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| WorkerOutboundQueueError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        let receipt = enqueue_in_transaction(&transaction, request, self.config)?;
        transaction
            .commit()
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        Ok(receipt)
    }

    /// Atomically extends a lease and retains its exact outbound control frame.
    /// The caller encodes the protocol frame; its envelope must match the renewal.
    /// A rejected renewal retains no frame. Any queue failure rolls back the lease
    /// and its replay receipt, so retry cannot lose the control message.
    ///
    /// # Errors
    ///
    /// Rejects mismatched envelopes, invalid queue authority, capacity exhaustion,
    /// changed-body replay, and storage failures.
    pub fn renew_lease_and_enqueue(
        &mut self,
        renewal: &ExecutionLeaseRenewal,
        frame: &WorkerOutboundEnqueueRequest,
    ) -> Result<ExecutionLeaseReceipt, WorkerOutboundQueueError> {
        let authority = &frame.authority;
        let slot = &authority.slot;
        if frame.message_id != renewal.message_id
            || frame.sent_at != renewal.sent_at
            || authority.lease_expires_at != renewal.expires_at
            || slot.worker_id != renewal.worker_id
            || slot.worker_instance_id != renewal.worker_instance_id
            || slot.job_id != renewal.job_id
            || slot.lease_id != renewal.lease_id
            || slot.attempt != renewal.attempt
            || slot.fencing_token != renewal.fencing_token
        {
            return Err(WorkerOutboundQueueError::invalid());
        }
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| WorkerOutboundQueueError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        let receipt = renew_execution_lease_in_transaction(&transaction, renewal)
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        if matches!(
            receipt.status,
            LeaseWriteStatus::Accepted | LeaseWriteStatus::Duplicate
        ) {
            enqueue_in_transaction(&transaction, frame, self.config)?;
        }
        transaction
            .commit()
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        Ok(receipt)
    }

    /// Claims a stable authority-bound page for one reconnected Worker.
    /// Claimed-but-unacknowledged rows are returned again with the same bytes.
    ///
    /// # Errors
    ///
    /// Rejects mismatched authority, an invalid cursor/page size, corrupt
    /// retained bytes, and storage failures.
    pub fn claim_page(
        &mut self,
        authority: &WorkerOutboundAuthority,
        observed_at: &Instant,
        cursor: Option<&WorkerOutboundPageCursor>,
        page_size: usize,
    ) -> Result<WorkerOutboundClaimPage, WorkerOutboundQueueError> {
        validate_authority(authority)?;
        validate_instant(observed_at)?;
        if !(1..=self.config.max_claim_page_size).contains(&page_size) {
            return Err(WorkerOutboundQueueError::invalid());
        }
        let authority_digest = authority_digest(authority)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| WorkerOutboundQueueError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        require_retained_slot_authority(&transaction, &authority.slot)?;
        let history = authority_history(&transaction, authority)?;
        let digests = history_digests(&history)?;
        let (after_sequence, snapshot_sequence) =
            page_cut(&transaction, &authority_digest, &digests, cursor, page_size)?;
        let mut rows = load_claim_rows(
            &transaction,
            &digests,
            after_sequence,
            snapshot_sequence,
            page_size.saturating_add(1),
        )?;
        let has_more = rows.len() > page_size;
        if has_more {
            rows.truncate(page_size);
        }
        let mut claims = Vec::with_capacity(rows.len());
        let mut last_sequence = None;
        for row in rows {
            if digest_bytes(&row.frame_bytes) != row.payload_digest {
                return Err(WorkerOutboundQueueError::storage());
            }
            let next_attempt = row
                .delivery_attempts
                .checked_add(1)
                .filter(|value| *value <= MAX_SAFE_INTEGER)
                .ok_or_else(WorkerOutboundQueueError::storage)?;
            let changed = transaction
                .execute(
                    "UPDATE internal_worker_outbound_messages
                     SET state = 'claimed', delivery_attempts = ?1, claimed_at = ?2
                     WHERE sequence = ?3 AND authority_digest = ?4",
                    params![
                        to_sql(next_attempt)?,
                        observed_at.0,
                        to_sql(row.sequence)?,
                        row.authority_digest,
                    ],
                )
                .map_err(|_| WorkerOutboundQueueError::storage())?;
            if changed != 1 {
                return Err(WorkerOutboundQueueError::storage());
            }
            last_sequence = Some(row.sequence);
            claims.push(WorkerOutboundClaim {
                authority: history
                    .get(&row.authority_digest)
                    .ok_or_else(WorkerOutboundQueueError::storage)?
                    .clone(),
                message_id: row.message_id,
                payload_digest: row.payload_digest,
                frame_bytes: row.frame_bytes,
                delivery_attempt: next_attempt,
                replayed: row.state == WorkerOutboundMessageState::Claimed,
            });
        }
        let next_cursor = if has_more {
            last_sequence.map(|after_sequence| WorkerOutboundPageCursor {
                authority_digest: authority_digest.clone(),
                after_sequence,
                snapshot_sequence,
                page_size,
            })
        } else {
            None
        };
        transaction
            .commit()
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        Ok(WorkerOutboundClaimPage {
            claims,
            next_cursor,
        })
    }

    /// Persists an authenticated remote confirmation before attempting cleanup.
    /// Non-interaction transport IDs return `false`; known foreign or unclaimed
    /// interaction IDs are rejected. No raw interaction content is copied.
    ///
    /// # Errors
    ///
    /// Rejects invalid identity, authority or a failure to persist the proof.
    pub fn confirm_remote(
        &mut self,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        message_id: &ExecutionMessageId,
        confirmed_at: &Instant,
    ) -> Result<bool, WorkerOutboundQueueError> {
        validate_message_id(message_id)?;
        validate_instant(confirmed_at)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| WorkerOutboundQueueError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        if let Some(proof) = load_confirmation(&transaction, message_id)? {
            validate_confirmation(&transaction, &proof, worker_id, worker_instance_id)?;
            transaction
                .commit()
                .map_err(|_| WorkerOutboundQueueError::storage())?;
            return Ok(true);
        }
        let retained: Option<(String, String, String, String, String, String)> = transaction.query_row(
            "SELECT worker_id, worker_instance_id, worker_session_id, authority_digest, payload_digest, state
             FROM internal_worker_outbound_messages WHERE message_id=?1",
            [&message_id.0], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))
            .optional().map_err(|_| WorkerOutboundQueueError::storage())?;
        let Some((worker, instance, session, stored_authority, payload, state)) = retained else {
            return Ok(false);
        };
        if worker != worker_id.0 || instance != worker_instance_id.0 {
            return Err(WorkerOutboundQueueError::authority());
        }
        if WorkerOutboundMessageState::parse(&state)? != WorkerOutboundMessageState::Claimed {
            return Err(WorkerOutboundQueueError::state());
        }
        let slot = load_slot_in_transaction(&transaction, &WorkerSessionId(session))?
            .ok_or_else(WorkerOutboundQueueError::authority)?
            .authority;
        let lease = load_lease_in_transaction(&transaction, &slot.job_id)
            .map_err(|_| WorkerOutboundQueueError::storage())?
            .ok_or_else(WorkerOutboundQueueError::authority)?;
        let authority = WorkerOutboundAuthority {
            slot,
            lease_issued_at: lease.issued_at,
            lease_expires_at: lease.expires_at,
        };
        validate_authority(&authority)?;
        if authority.slot.worker_id != *worker_id
            || authority.slot.worker_instance_id != *worker_instance_id
            || !authority_history(&transaction, &authority)?.contains_key(&stored_authority)
        {
            return Err(WorkerOutboundQueueError::authority());
        }
        require_retained_slot_authority(&transaction, &authority.slot)?;
        transaction.execute(
            "INSERT INTO internal_worker_outbound_confirmations
             (message_id,worker_id,worker_instance_id,authority_digest,authority_json,payload_digest,confirmed_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![message_id.0,worker_id.0,worker_instance_id.0,authority_digest(&authority)?,
                serde_json::to_vec(&authority).map_err(|_| WorkerOutboundQueueError::storage())?,payload,confirmed_at.0])
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        transaction
            .commit()
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        Ok(true)
    }

    /// Retries only proven remote confirmations, including after restart.
    /// Stops at the first cleanup failure; the proof remains until deletion and
    /// secure WAL checkpoint both complete. This performs no new execution.
    ///
    /// # Errors
    ///
    /// Rejects corrupt proof records or failure to load the bounded batch.
    pub fn retry_confirmed(
        &mut self,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
    ) -> Result<WorkerOutboundConfirmationProgress, WorkerOutboundQueueError> {
        let proofs = {
            let connection = self
                .storage
                .connection()
                .map_err(|_| WorkerOutboundQueueError::storage())?;
            let mut statement = connection
                .prepare(
                    "SELECT message_id FROM internal_worker_outbound_confirmations
                 WHERE worker_id=?1 AND worker_instance_id=?2 ORDER BY message_id LIMIT ?3",
                )
                .map_err(|_| WorkerOutboundQueueError::storage())?;
            let ids = statement
                .query_map(
                    params![
                        worker_id.0,
                        worker_instance_id.0,
                        i64::try_from(self.config.max_claim_page_size)
                            .map_err(|_| WorkerOutboundQueueError::invalid())?
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|_| WorkerOutboundQueueError::storage())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| WorkerOutboundQueueError::storage())?;
            let mut proofs = Vec::new();
            for id in ids {
                let mut proof = load_confirmation(connection, &ExecutionMessageId(id))?
                    .ok_or_else(WorkerOutboundQueueError::storage)?;
                validate_confirmation(connection, &proof, worker_id, worker_instance_id)?;
                // A later accepted renewal may extend the cleanup authority.
                // Preserve the original confirmation clock and immutable slot.
                if let Some(lease) =
                    load_lease_in_transaction(connection, &proof.authority.slot.job_id)
                        .map_err(|_| WorkerOutboundQueueError::storage())?
                    && lease.lease_id == proof.authority.slot.lease_id
                    && lease.issued_at == proof.authority.lease_issued_at
                    && lease.worker_id == proof.authority.slot.worker_id
                    && lease.worker_instance_id == proof.authority.slot.worker_instance_id
                    && lease.attempt == proof.authority.slot.attempt
                    && lease.fencing_token == proof.authority.slot.fencing_token
                {
                    let mut renewed = proof.authority.clone();
                    renewed.lease_expires_at = lease.expires_at;
                    authority_history(connection, &renewed)?;
                    proof.authority = renewed;
                }
                proofs.push(proof);
            }
            proofs
        };
        let mut progress = WorkerOutboundConfirmationProgress {
            acknowledgements: Vec::new(),
            retry_error: None,
        };
        for proof in proofs {
            let result = (|| {
                let slot = load_slot_in_transaction(
                    self.storage
                        .connection()
                        .map_err(|_| WorkerOutboundQueueError::storage())?,
                    &proof.authority.slot.worker_session_id,
                )?;
                if slot.is_some_and(|slot| {
                    slot.authority == proof.authority.slot
                        && matches!(
                            slot.state,
                            crate::WorkerSlotState::Completed
                                | crate::WorkerSlotState::Cancelled
                                | crate::WorkerSlotState::Failed
                                | crate::WorkerSlotState::RecoveryFailed
                        )
                }) {
                    self.settle_terminal(&proof.authority, &proof.confirmed_at)?;
                }
                self.acknowledge(&proof.authority, &proof.message_id, &proof.confirmed_at)
            })()
            .and_then(|receipt| {
                self.storage
                    .connection_mut()
                    .map_err(|_| WorkerOutboundQueueError::storage())?
                    .execute(
                        "DELETE FROM internal_worker_outbound_confirmations WHERE message_id=?1",
                        [&proof.message_id.0],
                    )
                    .map_err(|_| WorkerOutboundQueueError::storage())?;
                Ok(receipt)
            });
            match result {
                Ok(receipt) => progress.acknowledgements.push(receipt),
                Err(error) => {
                    progress.retry_error = Some(error.code());
                    break;
                }
            }
        }
        Ok(progress)
    }

    /// Acknowledges one claimed frame and atomically deletes its raw bytes.
    ///
    /// # Errors
    ///
    /// Rejects a foreign/stale authority, an unclaimed/missing message, or a
    /// storage failure. Exact acknowledgement replay remains successful.
    pub fn acknowledge(
        &mut self,
        authority: &WorkerOutboundAuthority,
        message_id: &ExecutionMessageId,
        acknowledged_at: &Instant,
    ) -> Result<WorkerOutboundAcknowledgement, WorkerOutboundQueueError> {
        validate_authority(authority)?;
        validate_message_id(message_id)?;
        validate_instant(acknowledged_at)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| WorkerOutboundQueueError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        let history = authority_history(&transaction, authority)?;
        if let Some(acknowledgement) = replay_ack(&transaction, message_id, &history)? {
            transaction
                .commit()
                .map_err(|_| WorkerOutboundQueueError::storage())?;
            secure_checkpoint(self.storage)?;
            return Ok(acknowledgement);
        }
        require_retained_slot_authority(&transaction, &authority.slot)?;
        let Some((stored_authority, payload_digest, state)) = transaction
            .query_row(
                "SELECT authority_digest, payload_digest, state
                 FROM internal_worker_outbound_messages WHERE message_id = ?1",
                [&message_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Sha256Digest(row.get(1)?),
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| WorkerOutboundQueueError::storage())?
        else {
            return Err(WorkerOutboundQueueError::state());
        };
        if !history.contains_key(&stored_authority) {
            return Err(WorkerOutboundQueueError::authority());
        }
        if WorkerOutboundMessageState::parse(&state)? != WorkerOutboundMessageState::Claimed {
            return Err(WorkerOutboundQueueError::state());
        }
        insert_settlement(
            &transaction,
            message_id,
            &stored_authority,
            &payload_digest,
            WorkerOutboundSettlement::Acknowledged,
            acknowledged_at,
        )?;
        delete_message(&transaction, message_id)?;
        transaction
            .commit()
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        secure_checkpoint(self.storage)?;
        Ok(WorkerOutboundAcknowledgement {
            message_id: message_id.clone(),
            settlement: WorkerOutboundSettlement::Acknowledged,
            replayed: false,
        })
    }

    /// Clears every raw frame after the exact slot becomes terminal.
    ///
    /// # Errors
    ///
    /// Rejects a foreign or non-terminal slot and storage failures.
    pub fn settle_terminal(
        &mut self,
        authority: &WorkerOutboundAuthority,
        settled_at: &Instant,
    ) -> Result<usize, WorkerOutboundQueueError> {
        validate_authority(authority)?;
        validate_instant(settled_at)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(|_| WorkerOutboundQueueError::storage())?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        require_terminal_slot_authority(&transaction, &authority.slot)?;
        let history = authority_history(&transaction, authority)?;
        let mut cleared = 0;
        for digest in history.keys() {
            let messages = load_settlement_rows(&transaction, digest)?;
            for (message_id, payload_digest) in &messages {
                insert_settlement(
                    &transaction,
                    message_id,
                    digest,
                    payload_digest,
                    WorkerOutboundSettlement::Terminal,
                    settled_at,
                )?;
                delete_message(&transaction, message_id)?;
            }
            cleared += messages.len();
        }
        transaction
            .commit()
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        secure_checkpoint(self.storage)?;
        Ok(cleared)
    }
}

struct RemoteConfirmation {
    message_id: ExecutionMessageId,
    worker_id: WorkerId,
    instance_id: WorkerInstanceId,
    authority: WorkerOutboundAuthority,
    authority_digest: String,
    payload_digest: String,
    confirmed_at: Instant,
}

fn load_confirmation(
    connection: &Connection,
    id: &ExecutionMessageId,
) -> Result<Option<RemoteConfirmation>, WorkerOutboundQueueError> {
    let stored = connection.query_row(
        "SELECT worker_id,worker_instance_id,authority_digest,authority_json,payload_digest,confirmed_at
         FROM internal_worker_outbound_confirmations WHERE message_id=?1", [&id.0],
        |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Vec<u8>>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?)))
        .optional().map_err(|_| WorkerOutboundQueueError::storage())?;
    stored
        .map(|(worker, instance, digest, json, payload, at)| {
            Ok(RemoteConfirmation {
                message_id: id.clone(),
                worker_id: WorkerId(worker),
                instance_id: WorkerInstanceId(instance),
                authority: serde_json::from_slice(&json)
                    .map_err(|_| WorkerOutboundQueueError::storage())?,
                authority_digest: digest,
                payload_digest: payload,
                confirmed_at: Instant(at),
            })
        })
        .transpose()
}

fn validate_confirmation(
    connection: &Connection,
    proof: &RemoteConfirmation,
    worker: &WorkerId,
    instance: &WorkerInstanceId,
) -> Result<(), WorkerOutboundQueueError> {
    validate_message_id(&proof.message_id)?;
    validate_authority(&proof.authority)?;
    validate_instant(&proof.confirmed_at)?;
    if &proof.worker_id != worker
        || &proof.instance_id != instance
        || proof.authority.slot.worker_id != *worker
        || proof.authority.slot.worker_instance_id != *instance
        || authority_digest(&proof.authority)? != proof.authority_digest
    {
        return Err(WorkerOutboundQueueError::authority());
    }
    let history = authority_history(connection, &proof.authority)?;
    let stored: Option<(String,String)> = connection.query_row(
        "SELECT authority_digest,payload_digest FROM internal_worker_outbound_messages WHERE message_id=?1
         UNION ALL SELECT authority_digest,payload_digest FROM internal_worker_outbound_settlements WHERE message_id=?1 LIMIT 1",
        [&proof.message_id.0], |row| Ok((row.get(0)?,row.get(1)?)))
        .optional().map_err(|_| WorkerOutboundQueueError::storage())?;
    if !stored.is_some_and(|(authority, payload)| {
        history.contains_key(&authority) && payload == proof.payload_digest
    }) || proof.confirmed_at.0 < proof.authority.lease_issued_at.0
    {
        return Err(WorkerOutboundQueueError::conflict());
    }
    Ok(())
}

struct StoredClaimRow {
    authority_digest: String,
    sequence: u64,
    message_id: ExecutionMessageId,
    payload_digest: Sha256Digest,
    frame_bytes: Vec<u8>,
    state: WorkerOutboundMessageState,
    delivery_attempts: u64,
}

fn validate_config(config: WorkerOutboundQueueConfig) -> Result<(), WorkerOutboundQueueError> {
    if !(1..=MAX_FRAME_BYTES).contains(&config.max_frame_bytes)
        || !(1..=MAX_QUEUE_CAPACITY).contains(&config.max_pending_messages_per_authority)
        || !(config.max_frame_bytes..=MAX_QUEUE_CAPACITY.saturating_mul(MAX_FRAME_BYTES))
            .contains(&config.max_retained_bytes)
        || !(1..=MAX_PAGE_SIZE).contains(&config.max_claim_page_size)
    {
        Err(WorkerOutboundQueueError::invalid())
    } else {
        Ok(())
    }
}

fn validate_enqueue(
    request: &WorkerOutboundEnqueueRequest,
    config: WorkerOutboundQueueConfig,
) -> Result<(), WorkerOutboundQueueError> {
    validate_authority(&request.authority)?;
    validate_message_id(&request.message_id)?;
    validate_instant(&request.sent_at)?;
    if request.frame_bytes.is_empty()
        || request.frame_bytes.len() > config.max_frame_bytes
        || digest_bytes(&request.frame_bytes) != request.payload_digest
        || request.sent_at.0 < request.authority.lease_issued_at.0
    {
        return Err(WorkerOutboundQueueError::invalid());
    }
    Ok(())
}

fn validate_authority(authority: &WorkerOutboundAuthority) -> Result<(), WorkerOutboundQueueError> {
    validate_instant(&authority.lease_issued_at)?;
    validate_instant(&authority.lease_expires_at)?;
    if authority.lease_issued_at.0 >= authority.lease_expires_at.0 {
        Err(WorkerOutboundQueueError::invalid())
    } else {
        Ok(())
    }
}

fn validate_message_id(message_id: &ExecutionMessageId) -> Result<(), WorkerOutboundQueueError> {
    let Some(suffix) = message_id.0.strip_prefix("xmsg_") else {
        return Err(WorkerOutboundQueueError::invalid());
    };
    if suffix.len() == 26
        && suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(
                    byte,
                    b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
                )
        })
    {
        Ok(())
    } else {
        Err(WorkerOutboundQueueError::invalid())
    }
}

fn validate_instant(value: &Instant) -> Result<(), WorkerOutboundQueueError> {
    let bytes = value.0.as_bytes();
    let valid = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(WorkerOutboundQueueError::invalid())
    }
}

fn authority_digest(
    authority: &WorkerOutboundAuthority,
) -> Result<String, WorkerOutboundQueueError> {
    let bytes = serde_json::to_vec(authority).map_err(|_| WorkerOutboundQueueError::invalid())?;
    Ok(digest_bytes(&bytes).0)
}

fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn replay_settlement(
    connection: &Connection,
    message_id: &ExecutionMessageId,
    authority_digest: &str,
    payload_digest: &Sha256Digest,
) -> Result<Option<WorkerOutboundEnqueueReceipt>, WorkerOutboundQueueError> {
    let stored = connection
        .query_row(
            "SELECT authority_digest, payload_digest, settlement
             FROM internal_worker_outbound_settlements WHERE message_id = ?1",
            [&message_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    let Some((stored_authority, stored_payload, settlement)) = stored else {
        return Ok(None);
    };
    if stored_authority != authority_digest || stored_payload != payload_digest.0 {
        return Err(WorkerOutboundQueueError::conflict());
    }
    Ok(Some(WorkerOutboundEnqueueReceipt {
        message_id: message_id.clone(),
        payload_digest: payload_digest.clone(),
        state: None,
        settlement: Some(WorkerOutboundSettlement::parse(&settlement)?),
        replayed: true,
    }))
}

fn replay_active(
    connection: &Connection,
    request: &WorkerOutboundEnqueueRequest,
    authority_digest: &str,
) -> Result<Option<WorkerOutboundEnqueueReceipt>, WorkerOutboundQueueError> {
    let stored = connection
        .query_row(
            "SELECT authority_digest, payload_digest, payload, state
             FROM internal_worker_outbound_messages WHERE message_id = ?1",
            [&request.message_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    let Some((stored_authority, stored_digest, stored_payload, state)) = stored else {
        return Ok(None);
    };
    if stored_authority != authority_digest
        || stored_digest != request.payload_digest.0
        || stored_payload != request.frame_bytes
    {
        return Err(WorkerOutboundQueueError::conflict());
    }
    Ok(Some(WorkerOutboundEnqueueReceipt {
        message_id: request.message_id.clone(),
        payload_digest: request.payload_digest.clone(),
        state: Some(WorkerOutboundMessageState::parse(&state)?),
        settlement: None,
        replayed: true,
    }))
}

fn page_cut(
    connection: &Connection,
    authority_digest: &str,
    digests: &str,
    cursor: Option<&WorkerOutboundPageCursor>,
    page_size: usize,
) -> Result<(u64, u64), WorkerOutboundQueueError> {
    if let Some(cursor) = cursor {
        if cursor.authority_digest != authority_digest
            || cursor.page_size != page_size
            || cursor.after_sequence > cursor.snapshot_sequence
        {
            return Err(WorkerOutboundQueueError::invalid());
        }
        return Ok((cursor.after_sequence, cursor.snapshot_sequence));
    }
    let snapshot = connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0)
             FROM internal_worker_outbound_messages WHERE authority_digest IN (SELECT value FROM json_each(?1))",
            [digests],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    Ok((
        0,
        u64::try_from(snapshot).map_err(|_| WorkerOutboundQueueError::storage())?,
    ))
}

fn load_claim_rows(
    connection: &Connection,
    digests: &str,
    after_sequence: u64,
    snapshot_sequence: u64,
    limit: usize,
) -> Result<Vec<StoredClaimRow>, WorkerOutboundQueueError> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, message_id, payload_digest, payload, state, delivery_attempts, authority_digest
             FROM internal_worker_outbound_messages
             WHERE authority_digest IN (SELECT value FROM json_each(?1)) AND sequence > ?2 AND sequence <= ?3
             AND NOT EXISTS (SELECT 1 FROM internal_worker_outbound_confirmations AS confirmation
                             WHERE confirmation.message_id=internal_worker_outbound_messages.message_id)
             ORDER BY sequence, message_id LIMIT ?4",
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    statement
        .query_map(
            params![
                digests,
                to_sql(after_sequence)?,
                to_sql(snapshot_sequence)?,
                i64::try_from(limit).map_err(|_| WorkerOutboundQueueError::invalid())?,
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?
        .map(|row| {
            let (
                sequence,
                message_id,
                payload_digest,
                frame_bytes,
                state,
                delivery_attempts,
                authority_digest,
            ) = row.map_err(|_| WorkerOutboundQueueError::storage())?;
            Ok(StoredClaimRow {
                authority_digest,
                sequence: u64::try_from(sequence)
                    .map_err(|_| WorkerOutboundQueueError::storage())?,
                message_id: ExecutionMessageId(message_id),
                payload_digest: Sha256Digest(payload_digest),
                frame_bytes,
                state: WorkerOutboundMessageState::parse(&state)?,
                delivery_attempts: u64::try_from(delivery_attempts)
                    .map_err(|_| WorkerOutboundQueueError::storage())?,
            })
        })
        .collect()
}

fn replay_ack(
    connection: &Connection,
    message_id: &ExecutionMessageId,
    history: &BTreeMap<String, WorkerOutboundAuthority>,
) -> Result<Option<WorkerOutboundAcknowledgement>, WorkerOutboundQueueError> {
    let stored = connection
        .query_row(
            "SELECT authority_digest, settlement
             FROM internal_worker_outbound_settlements WHERE message_id = ?1",
            [&message_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    let Some((stored_authority, settlement)) = stored else {
        return Ok(None);
    };
    if !history.contains_key(&stored_authority) {
        return Err(WorkerOutboundQueueError::authority());
    }
    Ok(Some(WorkerOutboundAcknowledgement {
        message_id: message_id.clone(),
        settlement: WorkerOutboundSettlement::parse(&settlement)?,
        replayed: true,
    }))
}

fn insert_settlement(
    connection: &Connection,
    message_id: &ExecutionMessageId,
    authority_digest: &str,
    payload_digest: &Sha256Digest,
    settlement: WorkerOutboundSettlement,
    settled_at: &Instant,
) -> Result<(), WorkerOutboundQueueError> {
    connection
        .execute(
            "INSERT INTO internal_worker_outbound_settlements
                (message_id, authority_digest, payload_digest, settlement, settled_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                message_id.0,
                authority_digest,
                payload_digest.0,
                settlement.as_str(),
                settled_at.0,
            ],
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    Ok(())
}

fn delete_message(
    connection: &Connection,
    message_id: &ExecutionMessageId,
) -> Result<(), WorkerOutboundQueueError> {
    let overwritten = connection
        .execute(
            "UPDATE internal_worker_outbound_messages
             SET payload = zeroblob(length(payload)) WHERE message_id = ?1",
            [&message_id.0],
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    if overwritten != 1 {
        return Err(WorkerOutboundQueueError::storage());
    }
    let changed = connection
        .execute(
            "DELETE FROM internal_worker_outbound_messages WHERE message_id = ?1",
            [&message_id.0],
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    if changed == 1 {
        Ok(())
    } else {
        Err(WorkerOutboundQueueError::storage())
    }
}

fn load_settlement_rows(
    connection: &Connection,
    authority_digest: &str,
) -> Result<Vec<(ExecutionMessageId, Sha256Digest)>, WorkerOutboundQueueError> {
    let mut statement = connection
        .prepare(
            "SELECT message_id, payload_digest FROM internal_worker_outbound_messages
             WHERE authority_digest = ?1 ORDER BY sequence, message_id",
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    statement
        .query_map([authority_digest], |row| {
            Ok((ExecutionMessageId(row.get(0)?), Sha256Digest(row.get(1)?)))
        })
        .map_err(|_| WorkerOutboundQueueError::storage())?
        .map(|row| row.map_err(|_| WorkerOutboundQueueError::storage()))
        .collect()
}

fn to_sql(value: u64) -> Result<i64, WorkerOutboundQueueError> {
    i64::try_from(value).map_err(|_| WorkerOutboundQueueError::invalid())
}

fn restrict_database_permissions(storage: &SqliteStorage) -> Result<(), WorkerOutboundQueueError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let database_path = storage.database_path();
        let parent = database_path
            .parent()
            .ok_or_else(WorkerOutboundQueueError::storage)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| WorkerOutboundQueueError::storage())?;
        fs::set_permissions(database_path, fs::Permissions::from_mode(0o600))
            .map_err(|_| WorkerOutboundQueueError::storage())?;
    }
    #[cfg(not(unix))]
    let _ = storage;
    Ok(())
}

fn secure_checkpoint(storage: &SqliteStorage) -> Result<(), WorkerOutboundQueueError> {
    let (busy, log_frames, _checkpointed_frames) = storage
        .connection()
        .map_err(|_| WorkerOutboundQueueError::storage())?
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    if busy == 0 && log_frames == 0 {
        Ok(())
    } else {
        Err(WorkerOutboundQueueError::storage())
    }
}

fn enqueue_in_transaction(
    connection: &Connection,
    request: &WorkerOutboundEnqueueRequest,
    config: WorkerOutboundQueueConfig,
) -> Result<WorkerOutboundEnqueueReceipt, WorkerOutboundQueueError> {
    validate_enqueue(request, config)?;
    let request_authority_digest = authority_digest(&request.authority)?;
    if let Some(receipt) = replay_settlement(
        connection,
        &request.message_id,
        &request_authority_digest,
        &request.payload_digest,
    )? {
        return Ok(receipt);
    }
    if let Some(receipt) = replay_active(connection, request, &request_authority_digest)? {
        return Ok(receipt);
    }
    let history = authority_history(connection, &request.authority)?;
    let current = load_lease_in_transaction(connection, &request.authority.slot.job_id)
        .map_err(|_| WorkerOutboundQueueError::storage())?
        .ok_or_else(WorkerOutboundQueueError::authority)?;
    let mut current_authority = request.authority.clone();
    current_authority
        .lease_expires_at
        .clone_from(&current.expires_at);
    if current.expires_at.0 < request.authority.lease_expires_at.0
        || !history.contains_key(&authority_digest(&current_authority)?)
    {
        return Err(WorkerOutboundQueueError::authority());
    }
    require_retained_slot_authority(connection, &current_authority.slot)?;
    let digests = history_digests(&history)?;
    let mut capacity_statement = connection
        .prepare(
            "SELECT
            COUNT(*) FILTER (WHERE authority_digest IN (SELECT value FROM json_each(?1))),
            COALESCE(SUM(length(payload)), 0)
         FROM internal_worker_outbound_messages",
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    let (authority_pending, retained_bytes) = capacity_statement
        .query_row([&digests], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    #[cfg(test)]
    crate::storage_mechanism_regression::capacity_query(
        capacity_statement.get_status(rusqlite::StatementStatus::VmStep),
    );
    let authority_pending =
        usize::try_from(authority_pending).map_err(|_| WorkerOutboundQueueError::storage())?;
    let retained_bytes =
        usize::try_from(retained_bytes).map_err(|_| WorkerOutboundQueueError::storage())?;
    let next_retained_bytes = retained_bytes
        .checked_add(request.frame_bytes.len())
        .ok_or_else(WorkerOutboundQueueError::capacity)?;
    if authority_pending >= config.max_pending_messages_per_authority
        || next_retained_bytes > config.max_retained_bytes
    {
        return Err(WorkerOutboundQueueError::capacity());
    }
    connection
        .execute(
            "INSERT INTO internal_worker_outbound_messages
                (message_id, authority_digest, worker_id, worker_instance_id,
                 worker_session_id, job_id, lease_id, attempt, fencing_token,
                 sent_at, payload_digest, payload, state, delivery_attempts, claimed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     'pending', 0, NULL)",
            params![
                request.message_id.0,
                request_authority_digest,
                request.authority.slot.worker_id.0,
                request.authority.slot.worker_instance_id.0,
                request.authority.slot.worker_session_id.0,
                request.authority.slot.job_id.0,
                request.authority.slot.lease_id.0,
                to_sql(request.authority.slot.attempt)?,
                request.authority.slot.fencing_token.0,
                request.sent_at.0,
                request.payload_digest.0,
                request.frame_bytes,
            ],
        )
        .map_err(|_| WorkerOutboundQueueError::storage())?;
    Ok(WorkerOutboundEnqueueReceipt {
        message_id: request.message_id.clone(),
        payload_digest: request.payload_digest.clone(),
        state: Some(WorkerOutboundMessageState::Pending),
        settlement: None,
        replayed: false,
    })
}

fn authority_history(
    connection: &Connection,
    authority: &WorkerOutboundAuthority,
) -> Result<BTreeMap<String, WorkerOutboundAuthority>, WorkerOutboundQueueError> {
    let slot = &authority.slot;
    let mut history = BTreeMap::new();
    // ponytail: scan one job's receipts; index by lease if long-lived jobs make this material.
    for lease in accepted_lease_history(connection, &slot.job_id)
        .map_err(|_| WorkerOutboundQueueError::storage())?
    {
        if lease.job_id != slot.job_id
            || lease.lease_id != slot.lease_id
            || lease.worker_id != slot.worker_id
            || lease.worker_instance_id != slot.worker_instance_id
            || lease.attempt != slot.attempt
            || lease.fencing_token != slot.fencing_token
            || lease.issued_at != authority.lease_issued_at
        {
            continue;
        }
        let mut historical = authority.clone();
        historical.lease_expires_at = lease.expires_at;
        history.insert(authority_digest(&historical)?, historical);
    }
    if !history.contains_key(&authority_digest(authority)?) {
        return Err(WorkerOutboundQueueError::authority());
    }
    Ok(history)
}

fn history_digests(
    history: &BTreeMap<String, WorkerOutboundAuthority>,
) -> Result<String, WorkerOutboundQueueError> {
    serde_json::to_string(&history.keys().collect::<Vec<_>>())
        .map_err(|_| WorkerOutboundQueueError::storage())
}
