// SPDX-License-Identifier: Apache-2.0

//! Durable `WorkerSession` slot ownership joined to existing Worker, lease, and
//! execution-admission authority.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, FencingToken, Instant, LeaseId, RequestId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};

use crate::{ExecutionLeaseRecord, SqliteStorage, StorageError};

const WORKER_SESSION_SLOT_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS worker_session_slot_resource_limits (
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    max_memory_bytes INTEGER NOT NULL CHECK (max_memory_bytes >= 0),
    max_disk_bytes INTEGER NOT NULL CHECK (max_disk_bytes >= 0),
    max_processes INTEGER NOT NULL CHECK (max_processes >= 0),
    PRIMARY KEY (worker_id, worker_instance_id)
);
CREATE TABLE IF NOT EXISTS worker_session_slots (
    worker_session_id TEXT PRIMARY KEY NOT NULL,
    codex_thread_id TEXT NOT NULL,
    job_id TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    lease_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0 AND attempt <= 1000),
    fencing_token TEXT NOT NULL,
    state TEXT NOT NULL CHECK (
        state IN ('running', 'cancelling', 'completed', 'cancelled', 'failed', 'recovery_failed')
    ),
    event_cursor INTEGER NOT NULL CHECK (event_cursor >= 0),
    memory_bytes INTEGER NOT NULL CHECK (memory_bytes >= 0),
    disk_bytes INTEGER NOT NULL CHECK (disk_bytes >= 0),
    process_slots INTEGER NOT NULL CHECK (process_slots > 0),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    opened_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    cancellation_requested_at TEXT,
    terminal_at TEXT,
    CHECK (
        (state = 'cancelling' AND cancellation_requested_at IS NOT NULL AND terminal_at IS NULL)
        OR (state IN ('completed', 'cancelled', 'failed', 'recovery_failed') AND terminal_at IS NOT NULL)
        OR (state = 'running' AND cancellation_requested_at IS NULL AND terminal_at IS NULL)
    )
);
CREATE TABLE IF NOT EXISTS worker_session_slot_receipts (
    operation TEXT NOT NULL,
    worker_session_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (operation, worker_session_id, request_id)
);
CREATE TABLE IF NOT EXISTS worker_session_recovery_receipts (
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (worker_id, worker_instance_id, request_id)
);
CREATE INDEX IF NOT EXISTS worker_session_slots_capacity
    ON worker_session_slots (worker_id, worker_instance_id, state);
CREATE UNIQUE INDEX IF NOT EXISTS worker_session_slots_active_job
    ON worker_session_slots (job_id)
    WHERE state IN ('running', 'cancelling');
CREATE UNIQUE INDEX IF NOT EXISTS worker_session_slots_active_thread
    ON worker_session_slots (codex_thread_id)
    WHERE state IN ('running', 'cancelling');
";

const MAX_SEQUENCE: u64 = 9_007_199_254_740_991;
const MAX_ATTEMPT: u64 = 1_000;

/// Per-process resource ceiling for all active `WorkerSession` slots.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotResourceLimits {
    pub max_memory_bytes: u64,
    pub max_disk_bytes: u64,
    pub max_processes: u64,
}

/// Resource reservation retained by one active slot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotResources {
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub process_slots: u64,
}

/// Exact authority required for every slot-local mutation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotAuthority {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: WorkerSessionId,
    pub codex_thread_id: CodexThreadId,
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub attempt: u64,
    pub fencing_token: FencingToken,
}

/// Durable slot lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSlotState {
    Running,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
    RecoveryFailed,
}

impl WorkerSlotState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::RecoveryFailed => "recovery_failed",
        }
    }

    const fn active(self) -> bool {
        matches!(self, Self::Running | Self::Cancelling)
    }

    fn parse(value: &str) -> Result<Self, WorkerSlotError> {
        match value {
            "running" => Ok(Self::Running),
            "cancelling" => Ok(Self::Cancelling),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            "recovery_failed" => Ok(Self::RecoveryFailed),
            _ => Err(WorkerSlotError::adapter(
                "stored Worker slot state is invalid",
            )),
        }
    }
}

/// Opens one slot after joining Worker, lease, and admission authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotOpenRequest {
    pub authority: WorkerSlotAuthority,
    pub resources: WorkerSlotResources,
    pub request_id: RequestId,
    pub opened_at: Instant,
}

/// Advances exactly one session-local event stream cursor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotEventAdvance {
    pub authority: WorkerSlotAuthority,
    pub request_id: RequestId,
    pub expected_cursor: u64,
    pub next_cursor: u64,
    pub observed_at: Instant,
}

/// Requests cancellation for exactly one active session slot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotCancellation {
    pub authority: WorkerSlotAuthority,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub requested_at: Instant,
}

/// Terminal result for one slot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotCloseRequest {
    pub authority: WorkerSlotAuthority,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub outcome: WorkerSlotState,
    pub closed_at: Instant,
}

/// Reconciles all old-process slots after a Worker process replacement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotRecoveryRequest {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub request_id: RequestId,
    pub recovered_at: Instant,
}

/// Complete durable `WorkerSession` slot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotRecord {
    pub authority: WorkerSlotAuthority,
    pub resources: WorkerSlotResources,
    pub state: WorkerSlotState,
    pub event_cursor: u64,
    pub revision: u64,
    pub opened_at: Instant,
    pub updated_at: Instant,
    pub cancellation_requested_at: Option<Instant>,
    pub terminal_at: Option<Instant>,
}

/// Replay-safe slot mutation result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotReceipt {
    pub slot: WorkerSlotRecord,
    pub replayed: bool,
}

/// Restart action applied to one old-process slot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum WorkerSlotRecoveryAction {
    Recovered { slot: WorkerSlotRecord },
    Failed { slot: WorkerSlotRecord },
}

/// Replay-safe, deterministic restart reconciliation result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerSlotRecoveryReceipt {
    pub actions: Vec<WorkerSlotRecoveryAction>,
    pub replayed: bool,
}

/// Current authoritative slot/resource capacity for one Worker process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerSlotCapacity {
    pub max_slots: u64,
    pub running_slots: u64,
    pub available_slots: u64,
    pub limits: WorkerSlotResourceLimits,
    pub reserved: WorkerSlotResources,
}

/// Stable slot failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerSlotErrorCode {
    InvalidInput,
    RequestConflict,
    RevisionConflict,
    CursorConflict,
    WorkerNotCurrent,
    WorkerNotHealthy,
    LeaseMismatch,
    LeaseExpired,
    AdmissionNotRunning,
    CapacityExhausted,
    ResourceExhausted,
    StateConflict,
    Adapter,
}

/// Worker slot failure with a stable machine-readable code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSlotError {
    code: WorkerSlotErrorCode,
    message: String,
}

impl WorkerSlotError {
    fn new(code: WorkerSlotErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(WorkerSlotErrorCode::InvalidInput, message)
    }

    fn adapter(message: impl Into<String>) -> Self {
        Self::new(WorkerSlotErrorCode::Adapter, message)
    }

    #[must_use]
    pub const fn code(&self) -> WorkerSlotErrorCode {
        self.code
    }
}

impl fmt::Display for WorkerSlotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkerSlotError {}

/// Durable `WorkerSession` slot manager.
pub struct WorkerSessionSlots<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the slot manager over existing registry/admission tables.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the slot schema cannot be prepared.
    pub fn worker_session_slots(&mut self) -> Result<WorkerSessionSlots<'_>, WorkerSlotError> {
        WorkerSessionSlots::new(self)
    }
}

impl<'storage> WorkerSessionSlots<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, WorkerSlotError> {
        let slots = Self { storage };
        ensure_worker_session_slot_schema(slots.storage.connection().map_err(storage_error)?)?;
        Ok(slots)
    }

    /// Configures immutable process-local resource limits. Exact repeats are
    /// idempotent; a changed limit is rejected.
    ///
    /// # Errors
    ///
    /// Rejects malformed/current-instance mismatches and `SQLite` failures.
    pub fn configure_resources(
        &mut self,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
        limits: WorkerSlotResourceLimits,
    ) -> Result<bool, WorkerSlotError> {
        validate_worker_identity(worker_id, worker_instance_id)?;
        validate_resource_limits(limits)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        require_current_worker(&transaction, worker_id, worker_instance_id, false)?;
        let existing = load_resource_limits(&transaction, worker_id, worker_instance_id)?;
        if let Some(existing) = existing {
            if existing != limits {
                return Err(WorkerSlotError::invalid(
                    "Worker slot resource limits are already configured differently",
                ));
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(false);
        }
        transaction
            .execute(
                "INSERT INTO worker_session_slot_resource_limits
                    (worker_id, worker_instance_id, max_memory_bytes, max_disk_bytes,
                     max_processes) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    worker_id.0,
                    worker_instance_id.0,
                    to_sql(limits.max_memory_bytes, "maxMemoryBytes")?,
                    to_sql(limits.max_disk_bytes, "maxDiskBytes")?,
                    to_sql(limits.max_processes, "maxProcesses")?,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(true)
    }

    /// Atomically opens one slot after checking current Worker, lease,
    /// execution-admission, slot-count, and local resource authority.
    ///
    /// # Errors
    ///
    /// Rejects identity/capacity/resource conflicts and `SQLite` failures.
    pub fn open(
        &mut self,
        request: &WorkerSlotOpenRequest,
    ) -> Result<WorkerSlotReceipt, WorkerSlotError> {
        validate_open(request)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_slot_receipt(
            &transaction,
            "open",
            &request.authority.worker_session_id,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let max_slots = require_current_worker(
            &transaction,
            &request.authority.worker_id,
            &request.authority.worker_instance_id,
            true,
        )?;
        require_lease_authority(&transaction, &request.authority, &request.opened_at)?;
        require_running_admission(&transaction, &request.authority.job_id)?;
        let limits = load_resource_limits(
            &transaction,
            &request.authority.worker_id,
            &request.authority.worker_instance_id,
        )?
        .ok_or_else(|| WorkerSlotError::invalid("Worker slot resource limits are missing"))?;
        let usage = active_usage(
            &transaction,
            &request.authority.worker_id,
            &request.authority.worker_instance_id,
        )?;
        if usage.running_slots >= max_slots {
            return Err(WorkerSlotError::new(
                WorkerSlotErrorCode::CapacityExhausted,
                "Worker has no available session slots",
            ));
        }
        ensure_resource_capacity(limits, usage.reserved, request.resources)?;
        if slot_identity_exists(&transaction, &request.authority)? {
            return Err(WorkerSlotError::invalid(
                "Worker slot identity is already durable",
            ));
        }
        insert_slot(&transaction, request)?;
        let slot = load_slot_in_transaction(&transaction, &request.authority.worker_session_id)?
            .ok_or_else(|| WorkerSlotError::adapter("opened Worker slot was not stored"))?;
        let receipt = WorkerSlotReceipt {
            slot,
            replayed: false,
        };
        insert_slot_receipt(
            &transaction,
            "open",
            &request.authority.worker_session_id,
            &request.request_id,
            &request_digest,
            &receipt,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    /// Advances one slot-local event cursor by exactly one position.
    ///
    /// # Errors
    ///
    /// Rejects cross-slot authority, gaps/duplicates, terminal slots, and
    /// `SQLite` failures.
    pub fn advance_event_cursor(
        &mut self,
        request: &WorkerSlotEventAdvance,
    ) -> Result<WorkerSlotReceipt, WorkerSlotError> {
        validate_event_advance(request)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_slot_receipt(
            &transaction,
            "event",
            &request.authority.worker_session_id,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let current = require_active_slot(&transaction, &request.authority)?;
        require_lease_authority(&transaction, &request.authority, &request.observed_at)?;
        if current.event_cursor != request.expected_cursor
            || request.next_cursor != request.expected_cursor + 1
        {
            return Err(WorkerSlotError::new(
                WorkerSlotErrorCode::CursorConflict,
                "Worker slot event cursor is not contiguous",
            ));
        }
        let changed = transaction
            .execute(
                "UPDATE worker_session_slots
                 SET event_cursor = ?1, revision = revision + 1, updated_at = ?2
                 WHERE worker_session_id = ?3 AND revision = ?4 AND event_cursor = ?5
                   AND state IN ('running', 'cancelling')",
                params![
                    to_sql(request.next_cursor, "nextCursor")?,
                    request.observed_at.0,
                    request.authority.worker_session_id.0,
                    to_sql(current.revision, "revision")?,
                    to_sql(request.expected_cursor, "expectedCursor")?,
                ],
            )
            .map_err(sql_error)?;
        if changed != 1 {
            return Err(WorkerSlotError::adapter(
                "Worker slot event cursor lost transaction authority",
            ));
        }
        finish_slot_mutation(
            &transaction,
            "event",
            &request.authority.worker_session_id,
            &request.request_id,
            &request_digest,
        )
        .and_then(|receipt| {
            transaction.commit().map_err(sql_error)?;
            Ok(receipt)
        })
    }

    /// Marks exactly one running slot as cancelling without affecting sibling
    /// slots or releasing its resources before acknowledgement.
    ///
    /// # Errors
    ///
    /// Rejects authority/revision/state conflicts and `SQLite` failures.
    pub fn request_cancellation(
        &mut self,
        request: &WorkerSlotCancellation,
    ) -> Result<WorkerSlotReceipt, WorkerSlotError> {
        validate_cancellation(request)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_slot_receipt(
            &transaction,
            "cancel",
            &request.authority.worker_session_id,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let current = require_active_slot(&transaction, &request.authority)?;
        require_lease_authority(&transaction, &request.authority, &request.requested_at)?;
        if current.revision != request.expected_revision {
            return Err(revision_conflict(
                request.expected_revision,
                current.revision,
            ));
        }
        if current.state != WorkerSlotState::Running {
            return Err(WorkerSlotError::new(
                WorkerSlotErrorCode::StateConflict,
                "Worker slot is not running",
            ));
        }
        require_time(&current, &request.requested_at)?;
        let changed = transaction
            .execute(
                "UPDATE worker_session_slots
                 SET state = 'cancelling', revision = revision + 1, updated_at = ?1,
                     cancellation_requested_at = ?1
                 WHERE worker_session_id = ?2 AND revision = ?3 AND state = 'running'",
                params![
                    request.requested_at.0,
                    request.authority.worker_session_id.0,
                    to_sql(request.expected_revision, "expectedRevision")?,
                ],
            )
            .map_err(sql_error)?;
        if changed != 1 {
            return Err(WorkerSlotError::adapter(
                "Worker slot cancellation lost transaction authority",
            ));
        }
        finish_slot_mutation(
            &transaction,
            "cancel",
            &request.authority.worker_session_id,
            &request.request_id,
            &request_digest,
        )
        .and_then(|receipt| {
            transaction.commit().map_err(sql_error)?;
            Ok(receipt)
        })
    }

    /// Closes one slot and releases its slot/resource capacity.
    ///
    /// # Errors
    ///
    /// Rejects authority/revision/outcome conflicts and `SQLite` failures.
    pub fn close(
        &mut self,
        request: &WorkerSlotCloseRequest,
    ) -> Result<WorkerSlotReceipt, WorkerSlotError> {
        validate_close(request)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_slot_receipt(
            &transaction,
            "close",
            &request.authority.worker_session_id,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let current = require_active_slot(&transaction, &request.authority)?;
        require_lease_authority(&transaction, &request.authority, &request.closed_at)?;
        if current.revision != request.expected_revision {
            return Err(revision_conflict(
                request.expected_revision,
                current.revision,
            ));
        }
        let legal = matches!(
            (current.state, request.outcome),
            (
                WorkerSlotState::Running,
                WorkerSlotState::Completed | WorkerSlotState::Failed
            ) | (WorkerSlotState::Cancelling, WorkerSlotState::Cancelled)
        );
        if !legal {
            return Err(WorkerSlotError::new(
                WorkerSlotErrorCode::StateConflict,
                "Worker slot terminal outcome is illegal",
            ));
        }
        require_time(&current, &request.closed_at)?;
        let changed = transaction
            .execute(
                "UPDATE worker_session_slots
                 SET state = ?1, revision = revision + 1, updated_at = ?2, terminal_at = ?2
                 WHERE worker_session_id = ?3 AND revision = ?4 AND state = ?5",
                params![
                    request.outcome.as_str(),
                    request.closed_at.0,
                    request.authority.worker_session_id.0,
                    to_sql(request.expected_revision, "expectedRevision")?,
                    current.state.as_str(),
                ],
            )
            .map_err(sql_error)?;
        if changed != 1 {
            return Err(WorkerSlotError::adapter(
                "Worker slot close lost transaction authority",
            ));
        }
        finish_slot_mutation(
            &transaction,
            "close",
            &request.authority.worker_session_id,
            &request.request_id,
            &request_digest,
        )
        .and_then(|receipt| {
            transaction.commit().map_err(sql_error)?;
            Ok(receipt)
        })
    }

    /// Reconciles every active slot owned by an older process instance. A slot
    /// is recovered only when the existing lease registry already holds a
    /// newer exact lease for the same Job and new Worker instance; otherwise
    /// it is durably marked `recovery_failed` and releases capacity.
    ///
    /// # Errors
    ///
    /// Rejects non-current Workers, changed replays, resource exhaustion, and
    /// `SQLite` failures.
    pub fn reconcile_restart(
        &mut self,
        request: &WorkerSlotRecoveryRequest,
    ) -> Result<WorkerSlotRecoveryReceipt, WorkerSlotError> {
        validate_recovery(request)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_recovery_receipt(
            &transaction,
            &request.worker_id,
            &request.worker_instance_id,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let max_slots = require_current_worker(
            &transaction,
            &request.worker_id,
            &request.worker_instance_id,
            false,
        )?;
        let limits = load_resource_limits(
            &transaction,
            &request.worker_id,
            &request.worker_instance_id,
        )?
        .ok_or_else(|| WorkerSlotError::invalid("Worker slot resource limits are missing"))?;
        let current_usage = active_usage(
            &transaction,
            &request.worker_id,
            &request.worker_instance_id,
        )?;
        let mut available_slots = max_slots.saturating_sub(current_usage.running_slots);
        let mut available_resources = subtract_resources(limits, current_usage.reserved)?;
        let mut old_slots = load_old_active_slots(
            &transaction,
            &request.worker_id,
            &request.worker_instance_id,
        )?;
        old_slots.sort_unstable_by(|left, right| {
            left.authority
                .worker_session_id
                .0
                .cmp(&right.authority.worker_session_id.0)
        });
        let mut actions = Vec::with_capacity(old_slots.len());
        for old in old_slots {
            require_time(&old, &request.recovered_at)?;
            let lease = load_current_lease(&transaction, &old.authority.job_id)?;
            let admission_running = running_admission(&transaction, &old.authority.job_id)?;
            let recoverable = lease.as_ref().is_some_and(|lease| {
                lease.worker_id == request.worker_id
                    && lease.worker_instance_id == request.worker_instance_id
                    && lease.attempt > old.authority.attempt
                    && greater_decimal(&lease.fencing_token.0, &old.authority.fencing_token.0)
                    && request.recovered_at.0 < lease.expires_at.0
                    && admission_running
                    && available_slots > 0
                    && resources_fit(available_resources, old.resources)
            });
            if recoverable {
                let Some(lease) = lease else {
                    return Err(WorkerSlotError::adapter(
                        "recoverable Worker slot has no current lease",
                    ));
                };
                update_recovered_slot(
                    &transaction,
                    &old,
                    &request.worker_instance_id,
                    &lease,
                    &request.recovered_at,
                )?;
                available_slots -= 1;
                available_resources = subtract_resources(available_resources, old.resources)?;
                let slot =
                    load_slot_in_transaction(&transaction, &old.authority.worker_session_id)?
                        .ok_or_else(|| WorkerSlotError::adapter("recovered slot was not stored"))?;
                actions.push(WorkerSlotRecoveryAction::Recovered { slot });
            } else {
                mark_recovery_failed(&transaction, &old, &request.recovered_at)?;
                let slot =
                    load_slot_in_transaction(&transaction, &old.authority.worker_session_id)?
                        .ok_or_else(|| WorkerSlotError::adapter("failed slot was not stored"))?;
                actions.push(WorkerSlotRecoveryAction::Failed { slot });
            }
        }
        let receipt = finish_recovery(&transaction, request, &request_digest, actions)?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    /// Loads one `WorkerSession` slot.
    ///
    /// # Errors
    ///
    /// Rejects malformed identity and `SQLite` failures.
    pub fn load(
        &self,
        worker_session_id: &WorkerSessionId,
    ) -> Result<Option<WorkerSlotRecord>, WorkerSlotError> {
        validate_id(&worker_session_id.0, "wsn_", "workerSessionId")?;
        load_slot_in_transaction(
            self.storage.connection().map_err(storage_error)?,
            worker_session_id,
        )
    }

    /// Returns authoritative capacity computed from durable active slots.
    ///
    /// # Errors
    ///
    /// Rejects non-current Workers, missing limits, corrupt usage, and
    /// `SQLite` failures.
    pub fn capacity(
        &self,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
    ) -> Result<WorkerSlotCapacity, WorkerSlotError> {
        validate_worker_identity(worker_id, worker_instance_id)?;
        let connection = self.storage.connection().map_err(storage_error)?;
        let max_slots = require_current_worker(connection, worker_id, worker_instance_id, false)?;
        let limits = load_resource_limits(connection, worker_id, worker_instance_id)?
            .ok_or_else(|| WorkerSlotError::invalid("Worker slot resource limits are missing"))?;
        let usage = active_usage(connection, worker_id, worker_instance_id)?;
        Ok(WorkerSlotCapacity {
            max_slots,
            running_slots: usage.running_slots,
            available_slots: max_slots.saturating_sub(usage.running_slots),
            limits,
            reserved: usage.reserved,
        })
    }
}

pub(crate) fn ensure_worker_session_slot_schema(
    connection: &Connection,
) -> Result<(), WorkerSlotError> {
    let legacy_unique_table = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'worker_session_slots'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error)?
        .is_some_and(|sql| {
            sql.contains("codex_thread_id TEXT UNIQUE") || sql.contains("job_id TEXT UNIQUE")
        });
    if legacy_unique_table {
        connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 BEGIN IMMEDIATE;
                 ALTER TABLE worker_session_slots RENAME TO worker_session_slots_legacy_unique;
                 CREATE TABLE worker_session_slots (
                     worker_session_id TEXT PRIMARY KEY NOT NULL,
                     codex_thread_id TEXT NOT NULL,
                     job_id TEXT NOT NULL,
                     worker_id TEXT NOT NULL,
                     worker_instance_id TEXT NOT NULL,
                     lease_id TEXT NOT NULL,
                     attempt INTEGER NOT NULL CHECK (attempt > 0 AND attempt <= 1000),
                     fencing_token TEXT NOT NULL,
                     state TEXT NOT NULL CHECK (
                         state IN ('running', 'cancelling', 'completed', 'cancelled', 'failed',
                                   'recovery_failed')
                     ),
                     event_cursor INTEGER NOT NULL CHECK (event_cursor >= 0),
                     memory_bytes INTEGER NOT NULL CHECK (memory_bytes >= 0),
                     disk_bytes INTEGER NOT NULL CHECK (disk_bytes >= 0),
                     process_slots INTEGER NOT NULL CHECK (process_slots > 0),
                     revision INTEGER NOT NULL CHECK (revision >= 1),
                     opened_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     cancellation_requested_at TEXT,
                     terminal_at TEXT,
                     CHECK (
                         (state = 'cancelling' AND cancellation_requested_at IS NOT NULL
                          AND terminal_at IS NULL)
                         OR (state IN ('completed', 'cancelled', 'failed', 'recovery_failed')
                             AND terminal_at IS NOT NULL)
                         OR (state = 'running' AND cancellation_requested_at IS NULL
                             AND terminal_at IS NULL)
                     )
                 );
                 INSERT INTO worker_session_slots (
                     worker_session_id, codex_thread_id, job_id, worker_id,
                     worker_instance_id, lease_id, attempt, fencing_token, state,
                     event_cursor, memory_bytes, disk_bytes, process_slots, revision,
                     opened_at, updated_at, cancellation_requested_at, terminal_at
                 ) SELECT
                     worker_session_id, codex_thread_id, job_id, worker_id,
                     worker_instance_id, lease_id, attempt, fencing_token, state,
                     event_cursor, memory_bytes, disk_bytes, process_slots, revision,
                     opened_at, updated_at, cancellation_requested_at, terminal_at
                   FROM worker_session_slots_legacy_unique;
                 DROP TABLE worker_session_slots_legacy_unique;
                 COMMIT;
                 PRAGMA foreign_keys = ON;",
            )
            .map_err(sql_error)?;
    }
    connection
        .execute_batch(WORKER_SESSION_SLOT_SCHEMA)
        .map_err(sql_error)
}

pub(crate) fn fence_worker_session_for_replacement_in_transaction(
    connection: &Connection,
    lease: &ExecutionLeaseRecord,
    replaced_at: &Instant,
) -> Result<(), WorkerSlotError> {
    let slot = connection
        .query_row(
            "SELECT worker_session_id, codex_thread_id, job_id, worker_id,
                    worker_instance_id, lease_id, attempt, fencing_token, state,
                    event_cursor, memory_bytes, disk_bytes, process_slots, revision,
                    opened_at, updated_at, cancellation_requested_at, terminal_at
             FROM worker_session_slots
             WHERE job_id = ?1 AND state IN ('running', 'cancelling')",
            [&lease.job_id.0],
            stored_slot_row,
        )
        .optional()
        .map_err(sql_error)?
        .map(complete_slot)
        .transpose()?;
    let Some(slot) = slot else {
        return Ok(());
    };
    if slot.authority.job_id != lease.job_id
        || slot.authority.lease_id != lease.lease_id
        || slot.authority.worker_id != lease.worker_id
        || slot.authority.worker_instance_id != lease.worker_instance_id
        || slot.authority.attempt != lease.attempt
        || slot.authority.fencing_token != lease.fencing_token
    {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::LeaseMismatch,
            "active Worker slot differs from the lease being replaced",
        ));
    }
    require_time(&slot, replaced_at)?;
    mark_recovery_failed(connection, &slot, replaced_at)
}

#[derive(Clone, Debug)]
struct CurrentLease {
    lease_id: LeaseId,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    attempt: u64,
    fencing_token: FencingToken,
    issued_at: Instant,
    expires_at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct SlotUsage {
    running_slots: u64,
    reserved: WorkerSlotResources,
}

fn require_current_worker(
    connection: &Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
    require_healthy: bool,
) -> Result<u64, WorkerSlotError> {
    let worker = connection
        .query_row(
            "SELECT worker_instance_id, health, max_slots
             FROM execution_workers WHERE worker_id = ?1",
            [&worker_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((stored_instance, health, max_slots)) = worker else {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::WorkerNotCurrent,
            "Worker is not registered",
        ));
    };
    if stored_instance != worker_instance_id.0 {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::WorkerNotCurrent,
            "Worker process instance is no longer current",
        ));
    }
    if require_healthy && health != "healthy" {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::WorkerNotHealthy,
            "Worker is not healthy",
        ));
    }
    from_sql(max_slots, "maxSlots")
}

fn require_lease_authority(
    connection: &Connection,
    authority: &WorkerSlotAuthority,
    observed_at: &Instant,
) -> Result<(), WorkerSlotError> {
    let Some(lease) = load_current_lease(connection, &authority.job_id)? else {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::LeaseMismatch,
            "execution lease does not exist",
        ));
    };
    if lease.lease_id != authority.lease_id
        || lease.worker_id != authority.worker_id
        || lease.worker_instance_id != authority.worker_instance_id
        || lease.attempt != authority.attempt
        || lease.fencing_token != authority.fencing_token
    {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::LeaseMismatch,
            "Worker slot authority does not match the current lease",
        ));
    }
    if observed_at.0 >= lease.expires_at.0 {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::LeaseExpired,
            "execution lease has expired",
        ));
    }
    Ok(())
}

fn load_current_lease(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<CurrentLease>, WorkerSlotError> {
    connection
        .query_row(
            "SELECT leases.lease_id, leases.worker_id, leases.worker_instance_id,
                    leases.attempt, leases.fencing_token, leases.issued_at, leases.expires_at
             FROM execution_leases AS leases
             WHERE leases.job_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM execution_lease_terminals AS terminals
                   WHERE terminals.lease_id = leases.lease_id
               )",
            [&job_id.0],
            |row| {
                let attempt = row.get::<_, i64>(3)?;
                Ok(CurrentLease {
                    lease_id: LeaseId(row.get(0)?),
                    worker_id: WorkerId(row.get(1)?),
                    worker_instance_id: WorkerInstanceId(row.get(2)?),
                    attempt: u64::try_from(attempt)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, attempt))?,
                    fencing_token: FencingToken(row.get(4)?),
                    issued_at: Instant(row.get(5)?),
                    expires_at: Instant(row.get(6)?),
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

fn require_running_admission(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<(), WorkerSlotError> {
    if running_admission(connection, job_id)? {
        Ok(())
    } else {
        Err(WorkerSlotError::new(
            WorkerSlotErrorCode::AdmissionNotRunning,
            "execution admission reservation is not running",
        ))
    }
}

fn running_admission(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<bool, WorkerSlotError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM execution_admission_reservations
             WHERE job_id = ?1 AND state = 'running'",
            [&job_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some())
}

fn load_resource_limits(
    connection: &Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<Option<WorkerSlotResourceLimits>, WorkerSlotError> {
    connection
        .query_row(
            "SELECT max_memory_bytes, max_disk_bytes, max_processes
             FROM worker_session_slot_resource_limits
             WHERE worker_id = ?1 AND worker_instance_id = ?2",
            params![worker_id.0, worker_instance_id.0],
            |row| {
                let memory = row.get::<_, i64>(0)?;
                let disk = row.get::<_, i64>(1)?;
                let processes = row.get::<_, i64>(2)?;
                Ok((memory, disk, processes))
            },
        )
        .optional()
        .map_err(sql_error)?
        .map(|(memory, disk, processes)| {
            Ok(WorkerSlotResourceLimits {
                max_memory_bytes: from_sql(memory, "maxMemoryBytes")?,
                max_disk_bytes: from_sql(disk, "maxDiskBytes")?,
                max_processes: from_sql(processes, "maxProcesses")?,
            })
        })
        .transpose()
}

fn active_usage(
    connection: &Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<SlotUsage, WorkerSlotError> {
    let mut statement = connection
        .prepare(
            "SELECT memory_bytes, disk_bytes, process_slots
             FROM worker_session_slots
             WHERE worker_id = ?1 AND worker_instance_id = ?2
               AND state IN ('running', 'cancelling')",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(params![worker_id.0, worker_instance_id.0], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(sql_error)?;
    let mut running_slots = 0_u64;
    let mut reserved = WorkerSlotResources {
        memory_bytes: 0,
        disk_bytes: 0,
        process_slots: 0,
    };
    for row in rows {
        let (memory, disk, processes) = row.map_err(sql_error)?;
        running_slots = checked_add(running_slots, 1, "active slot count")?;
        reserved.memory_bytes = checked_add(
            reserved.memory_bytes,
            from_sql(memory, "memoryBytes")?,
            "memory usage",
        )?;
        reserved.disk_bytes = checked_add(
            reserved.disk_bytes,
            from_sql(disk, "diskBytes")?,
            "disk usage",
        )?;
        reserved.process_slots = checked_add(
            reserved.process_slots,
            from_sql(processes, "processSlots")?,
            "process usage",
        )?;
    }
    Ok(SlotUsage {
        running_slots,
        reserved,
    })
}

fn ensure_resource_capacity(
    limits: WorkerSlotResourceLimits,
    used: WorkerSlotResources,
    requested: WorkerSlotResources,
) -> Result<(), WorkerSlotError> {
    if !resources_fit(
        WorkerSlotResourceLimits {
            max_memory_bytes: limits.max_memory_bytes.saturating_sub(used.memory_bytes),
            max_disk_bytes: limits.max_disk_bytes.saturating_sub(used.disk_bytes),
            max_processes: limits.max_processes.saturating_sub(used.process_slots),
        },
        requested,
    ) {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::ResourceExhausted,
            "Worker slot resources are exhausted",
        ));
    }
    Ok(())
}

const fn resources_fit(
    available: WorkerSlotResourceLimits,
    requested: WorkerSlotResources,
) -> bool {
    requested.memory_bytes <= available.max_memory_bytes
        && requested.disk_bytes <= available.max_disk_bytes
        && requested.process_slots <= available.max_processes
}

fn subtract_resources(
    available: WorkerSlotResourceLimits,
    resources: WorkerSlotResources,
) -> Result<WorkerSlotResourceLimits, WorkerSlotError> {
    Ok(WorkerSlotResourceLimits {
        max_memory_bytes: available
            .max_memory_bytes
            .checked_sub(resources.memory_bytes)
            .ok_or_else(|| WorkerSlotError::adapter("recovered memory exceeds limits"))?,
        max_disk_bytes: available
            .max_disk_bytes
            .checked_sub(resources.disk_bytes)
            .ok_or_else(|| WorkerSlotError::adapter("recovered disk exceeds limits"))?,
        max_processes: available
            .max_processes
            .checked_sub(resources.process_slots)
            .ok_or_else(|| WorkerSlotError::adapter("recovered processes exceed limits"))?,
    })
}

fn slot_identity_exists(
    connection: &Connection,
    authority: &WorkerSlotAuthority,
) -> Result<bool, WorkerSlotError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM worker_session_slots
             WHERE worker_session_id = ?1
                OR ((codex_thread_id = ?2 OR job_id = ?3)
                    AND state IN ('running', 'cancelling'))",
            params![
                authority.worker_session_id.0,
                authority.codex_thread_id.0,
                authority.job_id.0,
            ],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some())
}

fn insert_slot(
    connection: &Connection,
    request: &WorkerSlotOpenRequest,
) -> Result<(), WorkerSlotError> {
    connection
        .execute(
            "INSERT INTO worker_session_slots
                (worker_session_id, codex_thread_id, job_id, worker_id, worker_instance_id,
                 lease_id, attempt, fencing_token, state, event_cursor, memory_bytes,
                 disk_bytes, process_slots, revision, opened_at, updated_at,
                 cancellation_requested_at, terminal_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'running', 0, ?9, ?10, ?11,
                     1, ?12, ?12, NULL, NULL)",
            params![
                request.authority.worker_session_id.0,
                request.authority.codex_thread_id.0,
                request.authority.job_id.0,
                request.authority.worker_id.0,
                request.authority.worker_instance_id.0,
                request.authority.lease_id.0,
                to_sql(request.authority.attempt, "attempt")?,
                request.authority.fencing_token.0,
                to_sql(request.resources.memory_bytes, "memoryBytes")?,
                to_sql(request.resources.disk_bytes, "diskBytes")?,
                to_sql(request.resources.process_slots, "processSlots")?,
                request.opened_at.0,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

struct StoredSlotRow {
    worker_session_id: String,
    codex_thread_id: String,
    job_id: String,
    worker_id: String,
    worker_instance_id: String,
    lease_id: String,
    attempt: i64,
    fencing_token: String,
    state: String,
    event_cursor: i64,
    memory_bytes: i64,
    disk_bytes: i64,
    process_slots: i64,
    revision: i64,
    opened_at: String,
    updated_at: String,
    cancellation_requested_at: Option<String>,
    terminal_at: Option<String>,
}

fn stored_slot_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredSlotRow> {
    Ok(StoredSlotRow {
        worker_session_id: row.get(0)?,
        codex_thread_id: row.get(1)?,
        job_id: row.get(2)?,
        worker_id: row.get(3)?,
        worker_instance_id: row.get(4)?,
        lease_id: row.get(5)?,
        attempt: row.get(6)?,
        fencing_token: row.get(7)?,
        state: row.get(8)?,
        event_cursor: row.get(9)?,
        memory_bytes: row.get(10)?,
        disk_bytes: row.get(11)?,
        process_slots: row.get(12)?,
        revision: row.get(13)?,
        opened_at: row.get(14)?,
        updated_at: row.get(15)?,
        cancellation_requested_at: row.get(16)?,
        terminal_at: row.get(17)?,
    })
}

pub(crate) fn load_slot_in_transaction(
    connection: &Connection,
    worker_session_id: &WorkerSessionId,
) -> Result<Option<WorkerSlotRecord>, WorkerSlotError> {
    connection
        .query_row(
            "SELECT worker_session_id, codex_thread_id, job_id, worker_id,
                    worker_instance_id, lease_id, attempt, fencing_token, state,
                    event_cursor, memory_bytes, disk_bytes, process_slots, revision,
                    opened_at, updated_at, cancellation_requested_at, terminal_at
             FROM worker_session_slots WHERE worker_session_id = ?1",
            [&worker_session_id.0],
            stored_slot_row,
        )
        .optional()
        .map_err(sql_error)?
        .map(complete_slot)
        .transpose()
}

fn load_old_active_slots(
    connection: &Connection,
    worker_id: &WorkerId,
    current_instance_id: &WorkerInstanceId,
) -> Result<Vec<WorkerSlotRecord>, WorkerSlotError> {
    connection
        .prepare(
            "SELECT worker_session_id, codex_thread_id, job_id, worker_id,
                    worker_instance_id, lease_id, attempt, fencing_token, state,
                    event_cursor, memory_bytes, disk_bytes, process_slots, revision,
                    opened_at, updated_at, cancellation_requested_at, terminal_at
             FROM worker_session_slots
             WHERE worker_id = ?1 AND worker_instance_id != ?2
               AND state IN ('running', 'cancelling') ORDER BY worker_session_id",
        )
        .map_err(sql_error)?
        .query_map(params![worker_id.0, current_instance_id.0], stored_slot_row)
        .map_err(sql_error)?
        .map(|row| row.map_err(sql_error).and_then(complete_slot))
        .collect()
}

fn complete_slot(stored: StoredSlotRow) -> Result<WorkerSlotRecord, WorkerSlotError> {
    let slot = WorkerSlotRecord {
        authority: WorkerSlotAuthority {
            worker_id: WorkerId(stored.worker_id),
            worker_instance_id: WorkerInstanceId(stored.worker_instance_id),
            worker_session_id: WorkerSessionId(stored.worker_session_id),
            codex_thread_id: CodexThreadId(stored.codex_thread_id),
            job_id: ExecutionJobId(stored.job_id),
            lease_id: LeaseId(stored.lease_id),
            attempt: from_sql(stored.attempt, "attempt")?,
            fencing_token: FencingToken(stored.fencing_token),
        },
        resources: WorkerSlotResources {
            memory_bytes: from_sql(stored.memory_bytes, "memoryBytes")?,
            disk_bytes: from_sql(stored.disk_bytes, "diskBytes")?,
            process_slots: from_sql(stored.process_slots, "processSlots")?,
        },
        state: WorkerSlotState::parse(&stored.state)?,
        event_cursor: from_sql(stored.event_cursor, "eventCursor")?,
        revision: from_sql(stored.revision, "revision")?,
        opened_at: Instant(stored.opened_at),
        updated_at: Instant(stored.updated_at),
        cancellation_requested_at: stored.cancellation_requested_at.map(Instant),
        terminal_at: stored.terminal_at.map(Instant),
    };
    validate_stored_slot(&slot)?;
    Ok(slot)
}

fn require_active_slot(
    connection: &Connection,
    authority: &WorkerSlotAuthority,
) -> Result<WorkerSlotRecord, WorkerSlotError> {
    let slot = load_slot_in_transaction(connection, &authority.worker_session_id)?
        .ok_or_else(|| WorkerSlotError::invalid("Worker slot does not exist"))?;
    if slot.authority != *authority {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::LeaseMismatch,
            "Worker slot identity does not match durable authority",
        ));
    }
    if !slot.state.active() {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::StateConflict,
            "Worker slot is terminal",
        ));
    }
    Ok(slot)
}

pub(crate) fn require_running_slot_authority(
    connection: &Connection,
    authority: &WorkerSlotAuthority,
    lease_issued_at: &Instant,
    lease_expires_at: &Instant,
    observed_at: &Instant,
    require_healthy: bool,
) -> Result<WorkerSlotRecord, WorkerSlotError> {
    require_current_worker(
        connection,
        &authority.worker_id,
        &authority.worker_instance_id,
        require_healthy,
    )?;
    require_lease_authority(connection, authority, observed_at)?;
    let lease = load_current_lease(connection, &authority.job_id)?
        .ok_or_else(|| WorkerSlotError::invalid("execution lease does not exist"))?;
    if lease.issued_at != *lease_issued_at || lease.expires_at != *lease_expires_at {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::LeaseMismatch,
            "Worker slot lease time does not match durable authority",
        ));
    }
    let slot = require_active_slot(connection, authority)?;
    if slot.state != WorkerSlotState::Running {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::StateConflict,
            "Worker slot is not running",
        ));
    }
    Ok(slot)
}

pub(crate) fn require_terminal_slot_authority(
    connection: &Connection,
    authority: &WorkerSlotAuthority,
) -> Result<WorkerSlotRecord, WorkerSlotError> {
    let slot = load_slot_in_transaction(connection, &authority.worker_session_id)?
        .ok_or_else(|| WorkerSlotError::invalid("Worker slot does not exist"))?;
    if slot.authority != *authority {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::LeaseMismatch,
            "Worker slot identity does not match durable authority",
        ));
    }
    if slot.state.active() {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::StateConflict,
            "Worker slot is not terminal",
        ));
    }
    Ok(slot)
}

fn finish_slot_mutation(
    connection: &Connection,
    operation: &str,
    worker_session_id: &WorkerSessionId,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<WorkerSlotReceipt, WorkerSlotError> {
    let slot = load_slot_in_transaction(connection, worker_session_id)?
        .ok_or_else(|| WorkerSlotError::adapter("mutated Worker slot was not stored"))?;
    let receipt = WorkerSlotReceipt {
        slot,
        replayed: false,
    };
    insert_slot_receipt(
        connection,
        operation,
        worker_session_id,
        request_id,
        request_digest,
        &receipt,
    )?;
    Ok(receipt)
}

fn replay_slot_receipt(
    connection: &Connection,
    operation: &str,
    worker_session_id: &WorkerSessionId,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<Option<WorkerSlotReceipt>, WorkerSlotError> {
    let stored = connection
        .query_row(
            "SELECT request_digest, response_json FROM worker_session_slot_receipts
             WHERE operation = ?1 AND worker_session_id = ?2 AND request_id = ?3",
            params![operation, worker_session_id.0, request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((stored_digest, response_json)) = stored else {
        return Ok(None);
    };
    if stored_digest != request_digest {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::RequestConflict,
            "Worker slot request id was reused with another body",
        ));
    }
    let mut receipt: WorkerSlotReceipt = decode_json(&response_json)?;
    validate_stored_slot(&receipt.slot)?;
    receipt.replayed = true;
    Ok(Some(receipt))
}

fn insert_slot_receipt(
    connection: &Connection,
    operation: &str,
    worker_session_id: &WorkerSessionId,
    request_id: &RequestId,
    request_digest: &str,
    receipt: &WorkerSlotReceipt,
) -> Result<(), WorkerSlotError> {
    connection
        .execute(
            "INSERT INTO worker_session_slot_receipts
                (operation, worker_session_id, request_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                operation,
                worker_session_id.0,
                request_id.0,
                request_digest,
                encode_json(receipt)?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn update_recovered_slot(
    connection: &Connection,
    old: &WorkerSlotRecord,
    worker_instance_id: &WorkerInstanceId,
    lease: &CurrentLease,
    recovered_at: &Instant,
) -> Result<(), WorkerSlotError> {
    let changed = connection
        .execute(
            "UPDATE worker_session_slots
             SET worker_instance_id = ?1, lease_id = ?2, attempt = ?3,
                 fencing_token = ?4, revision = revision + 1, updated_at = ?5
             WHERE worker_session_id = ?6 AND worker_instance_id = ?7 AND revision = ?8
               AND state IN ('running', 'cancelling')",
            params![
                worker_instance_id.0,
                lease.lease_id.0,
                to_sql(lease.attempt, "attempt")?,
                lease.fencing_token.0,
                recovered_at.0,
                old.authority.worker_session_id.0,
                old.authority.worker_instance_id.0,
                to_sql(old.revision, "revision")?,
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(WorkerSlotError::adapter(
            "Worker slot recovery lost transaction authority",
        ));
    }
    Ok(())
}

fn mark_recovery_failed(
    connection: &Connection,
    old: &WorkerSlotRecord,
    recovered_at: &Instant,
) -> Result<(), WorkerSlotError> {
    let changed = connection
        .execute(
            "UPDATE worker_session_slots
             SET state = 'recovery_failed', revision = revision + 1,
                 updated_at = ?1, terminal_at = ?1
             WHERE worker_session_id = ?2 AND worker_instance_id = ?3 AND revision = ?4
               AND state IN ('running', 'cancelling')",
            params![
                recovered_at.0,
                old.authority.worker_session_id.0,
                old.authority.worker_instance_id.0,
                to_sql(old.revision, "revision")?,
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(WorkerSlotError::adapter(
            "Worker slot recovery failure lost transaction authority",
        ));
    }
    Ok(())
}

fn replay_recovery_receipt(
    connection: &Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<Option<WorkerSlotRecoveryReceipt>, WorkerSlotError> {
    let stored = connection
        .query_row(
            "SELECT request_digest, response_json FROM worker_session_recovery_receipts
             WHERE worker_id = ?1 AND worker_instance_id = ?2 AND request_id = ?3",
            params![worker_id.0, worker_instance_id.0, request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((stored_digest, response_json)) = stored else {
        return Ok(None);
    };
    if stored_digest != request_digest {
        return Err(WorkerSlotError::new(
            WorkerSlotErrorCode::RequestConflict,
            "Worker restart request id was reused with another body",
        ));
    }
    let mut receipt: WorkerSlotRecoveryReceipt = decode_json(&response_json)?;
    for action in &receipt.actions {
        let slot = match action {
            WorkerSlotRecoveryAction::Recovered { slot }
            | WorkerSlotRecoveryAction::Failed { slot } => slot,
        };
        validate_stored_slot(slot)?;
    }
    receipt.replayed = true;
    Ok(Some(receipt))
}

fn finish_recovery(
    connection: &Connection,
    request: &WorkerSlotRecoveryRequest,
    request_digest: &str,
    actions: Vec<WorkerSlotRecoveryAction>,
) -> Result<WorkerSlotRecoveryReceipt, WorkerSlotError> {
    let receipt = WorkerSlotRecoveryReceipt {
        actions,
        replayed: false,
    };
    insert_recovery_receipt(
        connection,
        &request.worker_id,
        &request.worker_instance_id,
        &request.request_id,
        request_digest,
        &receipt,
    )?;
    Ok(receipt)
}

fn insert_recovery_receipt(
    connection: &Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
    request_id: &RequestId,
    request_digest: &str,
    receipt: &WorkerSlotRecoveryReceipt,
) -> Result<(), WorkerSlotError> {
    connection
        .execute(
            "INSERT INTO worker_session_recovery_receipts
                (worker_id, worker_instance_id, request_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                worker_id.0,
                worker_instance_id.0,
                request_id.0,
                request_digest,
                encode_json(receipt)?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn validate_open(request: &WorkerSlotOpenRequest) -> Result<(), WorkerSlotError> {
    validate_authority(&request.authority)?;
    validate_request_id_time(&request.request_id, &request.opened_at)?;
    validate_resources(request.resources)
}

fn validate_event_advance(request: &WorkerSlotEventAdvance) -> Result<(), WorkerSlotError> {
    validate_authority(&request.authority)?;
    validate_request_id_time(&request.request_id, &request.observed_at)?;
    if request.expected_cursor > MAX_SEQUENCE
        || request.next_cursor > MAX_SEQUENCE
        || request.expected_cursor.checked_add(1) != Some(request.next_cursor)
    {
        return Err(WorkerSlotError::invalid(
            "Worker slot event cursor is outside the contiguous range",
        ));
    }
    Ok(())
}

fn validate_cancellation(request: &WorkerSlotCancellation) -> Result<(), WorkerSlotError> {
    validate_authority(&request.authority)?;
    validate_request_id_time(&request.request_id, &request.requested_at)?;
    validate_revision(request.expected_revision)
}

fn validate_close(request: &WorkerSlotCloseRequest) -> Result<(), WorkerSlotError> {
    validate_authority(&request.authority)?;
    validate_request_id_time(&request.request_id, &request.closed_at)?;
    validate_revision(request.expected_revision)?;
    if !matches!(
        request.outcome,
        WorkerSlotState::Completed | WorkerSlotState::Cancelled | WorkerSlotState::Failed
    ) {
        return Err(WorkerSlotError::invalid(
            "Worker slot close outcome is not terminal",
        ));
    }
    Ok(())
}

fn validate_recovery(request: &WorkerSlotRecoveryRequest) -> Result<(), WorkerSlotError> {
    validate_worker_identity(&request.worker_id, &request.worker_instance_id)?;
    validate_request_id_time(&request.request_id, &request.recovered_at)
}

fn validate_authority(authority: &WorkerSlotAuthority) -> Result<(), WorkerSlotError> {
    validate_worker_identity(&authority.worker_id, &authority.worker_instance_id)?;
    validate_id(&authority.worker_session_id.0, "wsn_", "workerSessionId")?;
    validate_id(&authority.codex_thread_id.0, "cdx_", "codexThreadId")?;
    validate_id(&authority.job_id.0, "job_", "jobId")?;
    validate_id(&authority.lease_id.0, "lse_", "leaseId")?;
    validate_fencing_token(&authority.fencing_token)?;
    if authority.attempt == 0 || authority.attempt > MAX_ATTEMPT {
        return Err(WorkerSlotError::invalid(
            "attempt is outside the supported range",
        ));
    }
    Ok(())
}

fn validate_worker_identity(
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<(), WorkerSlotError> {
    validate_id(&worker_id.0, "wrk_", "workerId")?;
    validate_id(&worker_instance_id.0, "wki_", "workerInstanceId")
}

fn validate_request_id_time(
    request_id: &RequestId,
    instant: &Instant,
) -> Result<(), WorkerSlotError> {
    validate_id(&request_id.0, "req_", "requestId")?;
    validate_instant(instant)
}

fn validate_revision(revision: u64) -> Result<(), WorkerSlotError> {
    if revision == 0 || revision > i64::MAX as u64 {
        Err(WorkerSlotError::invalid(
            "Worker slot revision is outside the supported range",
        ))
    } else {
        Ok(())
    }
}

fn validate_resource_limits(limits: WorkerSlotResourceLimits) -> Result<(), WorkerSlotError> {
    validate_sql_range(limits.max_memory_bytes, "maxMemoryBytes")?;
    validate_sql_range(limits.max_disk_bytes, "maxDiskBytes")?;
    validate_sql_range(limits.max_processes, "maxProcesses")
}

fn validate_resources(resources: WorkerSlotResources) -> Result<(), WorkerSlotError> {
    validate_sql_range(resources.memory_bytes, "memoryBytes")?;
    validate_sql_range(resources.disk_bytes, "diskBytes")?;
    if resources.process_slots == 0 {
        return Err(WorkerSlotError::invalid("processSlots must be positive"));
    }
    validate_sql_range(resources.process_slots, "processSlots")
}

fn validate_stored_slot(slot: &WorkerSlotRecord) -> Result<(), WorkerSlotError> {
    validate_authority(&slot.authority)
        .map_err(|_| WorkerSlotError::adapter("stored Worker slot authority is invalid"))?;
    validate_resources(slot.resources)
        .map_err(|_| WorkerSlotError::adapter("stored Worker slot resources are invalid"))?;
    for instant in [
        Some(&slot.opened_at),
        Some(&slot.updated_at),
        slot.cancellation_requested_at.as_ref(),
        slot.terminal_at.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_instant(instant)
            .map_err(|_| WorkerSlotError::adapter("stored Worker slot time is invalid"))?;
    }
    if slot.revision == 0
        || slot.event_cursor > MAX_SEQUENCE
        || slot.updated_at.0 < slot.opened_at.0
        || slot
            .terminal_at
            .as_ref()
            .is_some_and(|terminal| terminal.0 < slot.updated_at.0)
    {
        return Err(WorkerSlotError::adapter(
            "stored Worker slot values are invalid",
        ));
    }
    let lifecycle_valid = match slot.state {
        WorkerSlotState::Running => {
            slot.cancellation_requested_at.is_none() && slot.terminal_at.is_none()
        }
        WorkerSlotState::Cancelling => {
            slot.cancellation_requested_at.is_some() && slot.terminal_at.is_none()
        }
        WorkerSlotState::Completed | WorkerSlotState::Failed | WorkerSlotState::RecoveryFailed => {
            slot.terminal_at.is_some()
        }
        WorkerSlotState::Cancelled => {
            slot.cancellation_requested_at.is_some() && slot.terminal_at.is_some()
        }
    };
    if lifecycle_valid {
        Ok(())
    } else {
        Err(WorkerSlotError::adapter(
            "stored Worker slot lifecycle is inconsistent",
        ))
    }
}

fn require_time(slot: &WorkerSlotRecord, occurred_at: &Instant) -> Result<(), WorkerSlotError> {
    if occurred_at.0 < slot.updated_at.0 {
        Err(WorkerSlotError::invalid(
            "Worker slot operation precedes durable state",
        ))
    } else {
        Ok(())
    }
}

fn revision_conflict(expected: u64, actual: u64) -> WorkerSlotError {
    WorkerSlotError::new(
        WorkerSlotErrorCode::RevisionConflict,
        format!("expected Worker slot revision {expected}, but current revision is {actual}"),
    )
}

fn validate_id(value: &str, prefix: &str, field: &str) -> Result<(), WorkerSlotError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    });
    if valid {
        Ok(())
    } else {
        Err(WorkerSlotError::invalid(format!(
            "{field} is not canonical"
        )))
    }
}

fn validate_fencing_token(token: &FencingToken) -> Result<(), WorkerSlotError> {
    let value = token.0.as_bytes();
    if value.is_empty()
        || value.len() > 20
        || !value.iter().all(u8::is_ascii_digit)
        || (value.len() > 1 && value[0] == b'0')
        || token.0 == "0"
    {
        return Err(WorkerSlotError::invalid("fencingToken is not canonical"));
    }
    Ok(())
}

fn greater_decimal(left: &str, right: &str) -> bool {
    left.len() > right.len() || (left.len() == right.len() && left > right)
}

fn validate_instant(value: &Instant) -> Result<(), WorkerSlotError> {
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
        Err(WorkerSlotError::invalid(
            "Worker slot time is not canonical",
        ))
    }
}

fn checked_add(left: u64, right: u64, field: &str) -> Result<u64, WorkerSlotError> {
    left.checked_add(right)
        .ok_or_else(|| WorkerSlotError::adapter(format!("stored {field} is out of range")))
}

fn validate_sql_range(value: u64, field: &str) -> Result<(), WorkerSlotError> {
    if i64::try_from(value).is_ok() {
        Ok(())
    } else {
        Err(WorkerSlotError::invalid(format!(
            "{field} exceeds the supported range"
        )))
    }
}

fn to_sql(value: u64, field: &str) -> Result<i64, WorkerSlotError> {
    i64::try_from(value)
        .map_err(|_| WorkerSlotError::invalid(format!("{field} exceeds the supported range")))
}

fn from_sql(value: i64, field: &str) -> Result<u64, WorkerSlotError> {
    u64::try_from(value).map_err(|_| WorkerSlotError::adapter(format!("stored {field} is invalid")))
}

fn digest<T: Serialize>(value: &T) -> Result<String, WorkerSlotError> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        WorkerSlotError::adapter(format!("failed to encode Worker slot request: {error}"))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn encode_json<T: Serialize>(value: &T) -> Result<String, WorkerSlotError> {
    serde_json::to_string(value).map_err(|error| {
        WorkerSlotError::adapter(format!("failed to encode Worker slot value: {error}"))
    })
}

fn decode_json<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, WorkerSlotError> {
    serde_json::from_str(value).map_err(|error| {
        WorkerSlotError::adapter(format!("stored Worker slot value is corrupt: {error}"))
    })
}

#[allow(clippy::needless_pass_by_value)]
fn sql_error(error: rusqlite::Error) -> WorkerSlotError {
    WorkerSlotError::adapter(format!("Worker slot SQLite failure: {error}"))
}

#[allow(clippy::needless_pass_by_value)]
fn storage_error(error: StorageError) -> WorkerSlotError {
    WorkerSlotError::adapter(format!("Worker slot storage failure: {error}"))
}
