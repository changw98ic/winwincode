// SPDX-License-Identifier: Apache-2.0

//! Durable Worker registration, heartbeat, and execution-lease state.
//!
//! This module deliberately depends only on the shared domain value objects.
//! Generated `ExecutionPort` messages are decoded by the Control Plane adapter;
//! this store owns the durable authority that those adapters must consult.

use std::collections::HashSet;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, ExecutionSequence, FencingToken, Instant, LeaseId,
    RequestId, Sha256Digest, WorkerId, WorkerInstanceId, WorkerSessionId,
};

use crate::{SqliteStorage, StorageError};

const EXECUTION_REGISTRY_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS execution_workers (
    worker_id TEXT PRIMARY KEY NOT NULL,
    worker_instance_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    capabilities TEXT NOT NULL,
    last_heartbeat_at TEXT,
    heartbeat_sequence INTEGER NOT NULL CHECK (heartbeat_sequence >= 0),
    max_slots INTEGER NOT NULL CHECK (max_slots >= 0),
    available_slots INTEGER NOT NULL CHECK (available_slots >= 0)
);
CREATE TABLE IF NOT EXISTS execution_worker_instances (
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    PRIMARY KEY (worker_id, worker_instance_id)
);
CREATE TABLE IF NOT EXISTS execution_worker_registration_receipts (
    worker_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (worker_id, request_id)
);
CREATE TABLE IF NOT EXISTS execution_heartbeats (
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    heartbeat_sequence INTEGER NOT NULL CHECK (heartbeat_sequence > 0),
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (worker_id, worker_instance_id, heartbeat_sequence)
);
CREATE TABLE IF NOT EXISTS execution_leases (
    job_id TEXT PRIMARY KEY NOT NULL,
    lease_id TEXT UNIQUE NOT NULL,
    payload_digest TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0),
    fencing_token TEXT NOT NULL,
    issued_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS execution_lease_request_receipts (
    operation TEXT NOT NULL,
    job_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (operation, job_id, request_id)
);
CREATE INDEX IF NOT EXISTS execution_leases_worker_instance
    ON execution_leases (worker_id, worker_instance_id);
";

const MAX_EXECUTION_SEQUENCE: u64 = 9_007_199_254_740_991;
const MAX_LEASE_ATTEMPT: u64 = 1_000;
const MAX_ACTIVE_LEASES: usize = 1_024;
const MAX_WORKER_SLOTS: u64 = 1_024;

/// Status returned by Worker and lease mutations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseWriteStatus {
    Accepted,
    Duplicate,
    Gap,
    RejectedConflict,
    RejectedExpiredLease,
    RejectedStaleFencingToken,
    RejectedWorkerInstance,
}

/// Status returned by a Worker registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRegistrationStatus {
    Accepted,
    Duplicate,
    RejectedConflict,
}

/// Recovery required after registering a new process instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseRecovery {
    NoActiveLeases,
    ReacquireRequired,
}

/// Durable Worker identity and its latest heartbeat snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub started_at: Instant,
    pub capabilities: Vec<String>,
    pub last_heartbeat_at: Option<Instant>,
    pub heartbeat_sequence: u64,
    pub max_slots: u64,
    pub available_slots: u64,
}

/// Worker registration input independent of generated wire DTOs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerRegistrationRequest {
    pub capabilities: Vec<String>,
    pub message_id: ExecutionMessageId,
    pub request_id: RequestId,
    pub sent_at: Instant,
    pub started_at: Instant,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
}

/// Durable result of one Worker registration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerRegistrationReceipt {
    pub status: WorkerRegistrationStatus,
    pub lease_recovery: LeaseRecovery,
    pub worker: WorkerRecord,
}

/// A lease progress item reported by a Worker heartbeat.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActiveLeaseSummary {
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub attempt: u64,
    pub fencing_token: FencingToken,
}

/// Worker liveness input independent of generated wire DTOs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerHeartbeatRequest {
    pub active_leases: Vec<ActiveLeaseSummary>,
    pub available_slots: u64,
    pub heartbeat_sequence: ExecutionSequence,
    pub max_slots: u64,
    pub message_id: ExecutionMessageId,
    pub observed_at: Instant,
    pub sent_at: Instant,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
}

/// Durable result of one Worker heartbeat.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerHeartbeatReceipt {
    pub status: LeaseWriteStatus,
    pub next_sequence: u64,
    pub worker: Option<WorkerRecord>,
}

/// Scheduler input for claiming one already durable execution Job.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionLeaseClaim {
    pub expires_at: Instant,
    pub fencing_token: FencingToken,
    pub issued_at: Instant,
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub message_id: ExecutionMessageId,
    pub payload_digest: Sha256Digest,
    pub request_id: RequestId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub attempt: u64,
}

/// Scheduler input for renewing one exact current lease.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionLeaseRenewal {
    pub expires_at: Instant,
    pub fencing_token: FencingToken,
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub message_id: ExecutionMessageId,
    pub prior_expires_at: Instant,
    pub request_id: RequestId,
    pub sent_at: Instant,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub attempt: u64,
}

/// Durable authority for one Job attempt and Worker instance.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionLeaseRecord {
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub payload_digest: Sha256Digest,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub attempt: u64,
    pub fencing_token: FencingToken,
    pub issued_at: Instant,
    pub expires_at: Instant,
}

/// Durable result of a claim or renewal mutation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionLeaseReceipt {
    pub status: LeaseWriteStatus,
    pub lease: Option<ExecutionLeaseRecord>,
    pub replayed: bool,
}

/// Status returned after one Worker dispatch result is joined to durable
/// Worker and lease authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchResultStatus {
    Accepted,
    Duplicate,
    Conflict,
    RejectedCapacity,
    RejectedCapability,
    RejectedExpiredLease,
    RejectedStaleFencingToken,
    RejectedWorkerInstance,
}

/// Stable error code for a dispatch-result authority decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchResultErrorCode {
    MessageConflict,
    JobDispatchConflict,
    LeaseExpired,
    StaleFencingToken,
    WorkerNotRegistered,
    WorkerInstanceChanged,
}

/// Error details returned by the dispatch-result authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DispatchResultError {
    pub code: DispatchResultErrorCode,
    pub retryable: bool,
}

/// Durable result of one dispatch-result request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DispatchResultReceipt {
    pub status: DispatchResultStatus,
    pub error: Option<DispatchResultError>,
    pub replayed: bool,
}

/// Worker dispatch-result input independent of generated wire DTOs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchResultRequest {
    pub checked_at: Instant,
    pub expires_at: Instant,
    pub fencing_token: FencingToken,
    pub issued_at: Instant,
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub message_id: ExecutionMessageId,
    pub payload_digest: Sha256Digest,
    pub request_id: RequestId,
    pub sent_at: Instant,
    pub status: DispatchResultStatus,
    pub attempt: u64,
    /// Canonical serialized incoming error, if the Worker supplied one.
    ///
    /// The registry treats this as request data for idempotency only; the
    /// Control Plane owns its generated error DTO semantics.
    pub error: Option<String>,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: Option<WorkerSessionId>,
}

/// SQLite-backed Worker Registry and execution-lease authority.
pub struct ExecutionRegistry<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the Worker Registry view over this storage connection.
    ///
    /// The registry tables are created idempotently and are kept separate from
    /// the product-state schema so older product snapshots are never decoded as
    /// Worker authority.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the registry schema cannot be created.
    pub fn execution_registry(&mut self) -> Result<ExecutionRegistry<'_>, StorageError> {
        let registry = ExecutionRegistry { storage: self };
        registry.ensure_schema()?;
        Ok(registry)
    }
}

impl<'storage> ExecutionRegistry<'storage> {
    /// Creates the durable Worker tables if they do not exist yet.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the registry schema cannot be created.
    pub fn new(storage: &'storage mut SqliteStorage) -> Result<Self, StorageError> {
        let registry = Self { storage };
        registry.ensure_schema()?;
        Ok(registry)
    }

    /// Registers one stable Worker identity and one process instance.
    ///
    /// # Errors
    ///
    /// Returns an input error for a malformed request or an adapter error for
    /// a failed `SQLite` read/write.
    #[allow(clippy::too_many_lines)]
    pub fn register_worker(
        &mut self,
        request: &WorkerRegistrationRequest,
    ) -> Result<WorkerRegistrationReceipt, StorageError> {
        validate_registration(request)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;

        if let Some((stored_digest, response_json)) = transaction
            .query_row(
                "SELECT request_digest, response_json
                 FROM execution_worker_registration_receipts
                 WHERE worker_id = ?1 AND request_id = ?2",
                params![request.worker_id.0, request.request_id.0],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_error)?
        {
            if stored_digest != request_digest {
                let worker = load_worker_in_transaction(&transaction, &request.worker_id)?
                    .unwrap_or_else(|| empty_worker_record(request));
                transaction.commit().map_err(sql_error)?;
                return Ok(WorkerRegistrationReceipt {
                    status: WorkerRegistrationStatus::RejectedConflict,
                    lease_recovery: LeaseRecovery::NoActiveLeases,
                    worker,
                });
            }
            let mut response = decode_json::<WorkerRegistrationReceipt>(&response_json)?;
            validate_stored_worker(&response.worker)?;
            response.status = WorkerRegistrationStatus::Duplicate;
            transaction.commit().map_err(sql_error)?;
            return Ok(response);
        }

        let prior = load_worker_in_transaction(&transaction, &request.worker_id)?;
        let instance_seen = worker_instance_seen(
            &transaction,
            &request.worker_id,
            &request.worker_instance_id,
        )?;
        if instance_seen
            && prior
                .as_ref()
                .is_none_or(|worker| worker.worker_instance_id != request.worker_instance_id)
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerRegistrationReceipt {
                status: WorkerRegistrationStatus::RejectedConflict,
                lease_recovery: LeaseRecovery::NoActiveLeases,
                worker: prior.unwrap_or_else(|| empty_worker_record(request)),
            });
        }
        if let Some(existing) = prior.as_ref()
            && existing.worker_instance_id == request.worker_instance_id
            && existing.started_at != request.started_at
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerRegistrationReceipt {
                status: WorkerRegistrationStatus::RejectedConflict,
                lease_recovery: LeaseRecovery::NoActiveLeases,
                worker: existing.clone(),
            });
        }
        let lease_recovery = if prior
            .as_ref()
            .is_some_and(|worker| worker.worker_instance_id != request.worker_instance_id)
            && has_old_worker_leases(
                &transaction,
                &request.worker_id,
                &request.worker_instance_id,
            )? {
            LeaseRecovery::ReacquireRequired
        } else {
            LeaseRecovery::NoActiveLeases
        };
        let heartbeat_sequence = prior
            .as_ref()
            .filter(|worker| worker.worker_instance_id == request.worker_instance_id)
            .map_or(0, |worker| worker.heartbeat_sequence);
        let last_heartbeat_at = prior
            .as_ref()
            .filter(|worker| worker.worker_instance_id == request.worker_instance_id)
            .and_then(|worker| worker.last_heartbeat_at.clone());
        let worker = WorkerRecord {
            worker_id: request.worker_id.clone(),
            worker_instance_id: request.worker_instance_id.clone(),
            started_at: request.started_at.clone(),
            capabilities: request.capabilities.clone(),
            last_heartbeat_at,
            heartbeat_sequence,
            max_slots: prior
                .as_ref()
                .filter(|worker| worker.worker_instance_id == request.worker_instance_id)
                .map_or(0, |worker| worker.max_slots),
            available_slots: prior
                .as_ref()
                .filter(|worker| worker.worker_instance_id == request.worker_instance_id)
                .map_or(0, |worker| worker.available_slots),
        };
        let response = WorkerRegistrationReceipt {
            status: WorkerRegistrationStatus::Accepted,
            lease_recovery,
            worker: worker.clone(),
        };
        let capabilities = encode_json(&worker.capabilities)?;
        let last_heartbeat_at = worker
            .last_heartbeat_at
            .as_ref()
            .map(|value| value.0.as_str());
        transaction
            .execute(
                "INSERT INTO execution_workers
                    (worker_id, worker_instance_id, started_at, capabilities,
                     last_heartbeat_at, heartbeat_sequence, max_slots, available_slots)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(worker_id) DO UPDATE SET
                    worker_instance_id = excluded.worker_instance_id,
                    started_at = excluded.started_at,
                    capabilities = excluded.capabilities,
                    last_heartbeat_at = excluded.last_heartbeat_at,
                    heartbeat_sequence = excluded.heartbeat_sequence,
                    max_slots = excluded.max_slots,
                    available_slots = excluded.available_slots",
                params![
                    worker.worker_id.0,
                    worker.worker_instance_id.0,
                    worker.started_at.0,
                    capabilities,
                    last_heartbeat_at,
                    i64::try_from(worker.heartbeat_sequence).map_err(|_| {
                        StorageError::invalid_input("heartbeat sequence is out of range")
                    })?,
                    i64::try_from(worker.max_slots)
                        .map_err(|_| StorageError::invalid_input("max slots is out of range"))?,
                    i64::try_from(worker.available_slots).map_err(|_| {
                        StorageError::invalid_input("available slots is out of range")
                    })?,
                ],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO execution_worker_instances
                    (worker_id, worker_instance_id, started_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(worker_id, worker_instance_id) DO NOTHING",
                params![
                    worker.worker_id.0,
                    worker.worker_instance_id.0,
                    worker.started_at.0,
                ],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO execution_worker_registration_receipts
                    (worker_id, request_id, request_digest, response_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    request.worker_id.0,
                    request.request_id.0,
                    request_digest,
                    encode_json(&response)?,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(response)
    }

    /// Applies one contiguous Worker heartbeat without creating a Job dispatch.
    ///
    /// # Errors
    ///
    /// Returns an input error for a malformed request or an adapter error for
    /// a failed `SQLite` read/write.
    #[allow(clippy::too_many_lines)]
    pub fn record_heartbeat(
        &mut self,
        request: &WorkerHeartbeatRequest,
    ) -> Result<WorkerHeartbeatReceipt, StorageError> {
        validate_heartbeat(request)?;
        let sequence = sequence_value(&request.heartbeat_sequence, "heartbeat sequence")?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let Some(current) = load_worker_in_transaction(&transaction, &request.worker_id)? else {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerHeartbeatReceipt {
                status: LeaseWriteStatus::RejectedWorkerInstance,
                next_sequence: 1,
                worker: None,
            });
        };
        if current.worker_instance_id != request.worker_instance_id {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerHeartbeatReceipt {
                status: LeaseWriteStatus::RejectedWorkerInstance,
                next_sequence: current.heartbeat_sequence.saturating_add(1),
                worker: Some(current),
            });
        }
        if let Some((stored_digest, response_json)) = transaction
            .query_row(
                "SELECT request_digest, response_json
                 FROM execution_heartbeats
                 WHERE worker_id = ?1 AND worker_instance_id = ?2 AND heartbeat_sequence = ?3",
                params![
                    request.worker_id.0,
                    request.worker_instance_id.0,
                    i64::try_from(sequence).map_err(|_| StorageError::invalid_input(
                        "heartbeat sequence is out of range"
                    ))?,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(sql_error)?
        {
            if stored_digest != request_digest {
                transaction.commit().map_err(sql_error)?;
                return Ok(WorkerHeartbeatReceipt {
                    status: LeaseWriteStatus::RejectedConflict,
                    next_sequence: current.heartbeat_sequence.saturating_add(1),
                    worker: Some(current),
                });
            }
            let mut response = decode_json::<WorkerHeartbeatReceipt>(&response_json)?;
            if let Some(worker) = response.worker.as_ref() {
                validate_stored_worker(worker)?;
            }
            response.status = LeaseWriteStatus::Duplicate;
            transaction.commit().map_err(sql_error)?;
            return Ok(response);
        }

        let expected = current.heartbeat_sequence.saturating_add(1);
        if sequence > expected {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerHeartbeatReceipt {
                status: LeaseWriteStatus::Gap,
                next_sequence: expected,
                worker: Some(current),
            });
        }
        if sequence != expected {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerHeartbeatReceipt {
                status: LeaseWriteStatus::RejectedConflict,
                next_sequence: expected,
                worker: Some(current),
            });
        }

        if current
            .last_heartbeat_at
            .as_ref()
            .is_some_and(|last| request.observed_at.0 < last.0)
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerHeartbeatReceipt {
                status: LeaseWriteStatus::RejectedConflict,
                next_sequence: expected,
                worker: Some(current),
            });
        }
        if let Some(status) = heartbeat_lease_status(&transaction, request)? {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerHeartbeatReceipt {
                status,
                next_sequence: expected,
                worker: Some(current),
            });
        }

        let worker = WorkerRecord {
            worker_id: current.worker_id.clone(),
            worker_instance_id: current.worker_instance_id.clone(),
            started_at: current.started_at,
            capabilities: current.capabilities,
            last_heartbeat_at: Some(request.observed_at.clone()),
            heartbeat_sequence: sequence,
            max_slots: request.max_slots,
            available_slots: request.available_slots,
        };
        transaction
            .execute(
                "UPDATE execution_workers
                 SET last_heartbeat_at = ?1, heartbeat_sequence = ?2,
                     max_slots = ?3, available_slots = ?4
                 WHERE worker_id = ?5 AND worker_instance_id = ?6",
                params![
                    request.observed_at.0,
                    i64::try_from(sequence).map_err(|_| StorageError::invalid_input(
                        "heartbeat sequence is out of range"
                    ))?,
                    i64::try_from(request.max_slots)
                        .map_err(|_| StorageError::invalid_input("max slots is out of range"))?,
                    i64::try_from(request.available_slots).map_err(|_| {
                        StorageError::invalid_input("available slots is out of range")
                    })?,
                    request.worker_id.0,
                    request.worker_instance_id.0,
                ],
            )
            .map_err(sql_error)?;
        let response = WorkerHeartbeatReceipt {
            status: LeaseWriteStatus::Accepted,
            next_sequence: sequence.saturating_add(1),
            worker: Some(worker),
        };
        transaction
            .execute(
                "INSERT INTO execution_heartbeats
                    (worker_id, worker_instance_id, heartbeat_sequence, request_digest, response_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    request.worker_id.0,
                    request.worker_instance_id.0,
                    i64::try_from(sequence)
                        .map_err(|_| StorageError::invalid_input("heartbeat sequence is out of range"))?,
                    request_digest,
                    encode_json(&response)?,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(response)
    }

    /// Claims one durable Job for the currently registered Worker instance.
    ///
    /// # Errors
    ///
    /// Returns an input error for a malformed request or an adapter error for
    /// a failed `SQLite` read/write.
    #[allow(clippy::too_many_lines)]
    pub fn claim_execution_job(
        &mut self,
        request: &ExecutionLeaseClaim,
    ) -> Result<ExecutionLeaseReceipt, StorageError> {
        validate_claim(request)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = lease_request_replay(
            &transaction,
            "claim",
            &request.job_id,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        if !worker_instance_is_current(
            &transaction,
            &request.worker_id,
            &request.worker_instance_id,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedWorkerInstance,
                None,
                false,
            ));
        }
        if transaction
            .query_row(
                "SELECT 1 FROM execution_leases WHERE lease_id = ?1 AND job_id != ?2",
                params![request.lease_id.0, request.job_id.0],
                |_| Ok(()),
            )
            .optional()
            .map_err(sql_error)?
            .is_some()
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                None,
                false,
            ));
        }

        if let Some(current) = load_lease_in_transaction(&transaction, &request.job_id)? {
            let expired = request.issued_at.0 >= current.expires_at.0;
            if less_decimal(&request.fencing_token.0, &current.fencing_token.0) {
                transaction.commit().map_err(sql_error)?;
                return Ok(lease_receipt(
                    LeaseWriteStatus::RejectedStaleFencingToken,
                    Some(current),
                    false,
                ));
            }
            if !expired {
                transaction.commit().map_err(sql_error)?;
                return Ok(lease_receipt(
                    LeaseWriteStatus::RejectedConflict,
                    Some(current),
                    false,
                ));
            }
            if request.attempt <= current.attempt {
                transaction.commit().map_err(sql_error)?;
                return Ok(lease_receipt(
                    LeaseWriteStatus::RejectedExpiredLease,
                    Some(current),
                    false,
                ));
            }
            if request.lease_id == current.lease_id {
                transaction.commit().map_err(sql_error)?;
                return Ok(lease_receipt(
                    LeaseWriteStatus::RejectedConflict,
                    Some(current),
                    false,
                ));
            }
            if request.fencing_token == current.fencing_token {
                transaction.commit().map_err(sql_error)?;
                return Ok(lease_receipt(
                    LeaseWriteStatus::RejectedConflict,
                    Some(current),
                    false,
                ));
            }
        }

        let lease = ExecutionLeaseRecord {
            job_id: request.job_id.clone(),
            lease_id: request.lease_id.clone(),
            payload_digest: request.payload_digest.clone(),
            worker_id: request.worker_id.clone(),
            worker_instance_id: request.worker_instance_id.clone(),
            attempt: request.attempt,
            fencing_token: request.fencing_token.clone(),
            issued_at: request.issued_at.clone(),
            expires_at: request.expires_at.clone(),
        };
        let response = lease_receipt(LeaseWriteStatus::Accepted, Some(lease.clone()), false);
        transaction
            .execute(
                "INSERT INTO execution_leases
                    (job_id, lease_id, payload_digest, worker_id, worker_instance_id,
                     attempt, fencing_token, issued_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(job_id) DO UPDATE SET
                    lease_id = excluded.lease_id,
                    payload_digest = excluded.payload_digest,
                    worker_id = excluded.worker_id,
                    worker_instance_id = excluded.worker_instance_id,
                    attempt = excluded.attempt,
                    fencing_token = excluded.fencing_token,
                    issued_at = excluded.issued_at,
                    expires_at = excluded.expires_at",
                params![
                    request.job_id.0,
                    request.lease_id.0,
                    request.payload_digest.0,
                    request.worker_id.0,
                    request.worker_instance_id.0,
                    i64::try_from(request.attempt)
                        .map_err(|_| StorageError::invalid_input("attempt is out of range"))?,
                    request.fencing_token.0,
                    request.issued_at.0,
                    request.expires_at.0,
                ],
            )
            .map_err(sql_error)?;
        insert_lease_request_receipt(
            &transaction,
            "claim",
            &request.job_id,
            &request.request_id,
            &request_digest,
            &response,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(response)
    }

    /// Renews one exact current lease without changing its attempt or fence.
    ///
    /// # Errors
    ///
    /// Returns an input error for a malformed request or an adapter error for
    /// a failed `SQLite` read/write.
    #[allow(clippy::too_many_lines)]
    pub fn renew_execution_lease(
        &mut self,
        request: &ExecutionLeaseRenewal,
    ) -> Result<ExecutionLeaseReceipt, StorageError> {
        validate_renewal(request)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = lease_request_replay(
            &transaction,
            "renew",
            &request.job_id,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        if !worker_instance_is_current(
            &transaction,
            &request.worker_id,
            &request.worker_instance_id,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedWorkerInstance,
                None,
                false,
            ));
        }
        let Some(current) = load_lease_in_transaction(&transaction, &request.job_id)? else {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                None,
                false,
            ));
        };
        if less_decimal(&request.fencing_token.0, &current.fencing_token.0) {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedStaleFencingToken,
                Some(current),
                false,
            ));
        }
        if request.worker_id != current.worker_id
            || request.worker_instance_id != current.worker_instance_id
            || request.lease_id != current.lease_id
            || request.attempt != current.attempt
            || request.fencing_token != current.fencing_token
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                Some(current),
                false,
            ));
        }
        if request.sent_at.0 >= current.expires_at.0 {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedExpiredLease,
                Some(current),
                false,
            ));
        }
        if request.prior_expires_at != current.expires_at {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                Some(current),
                false,
            ));
        }
        if request.expires_at.0 <= current.expires_at.0 {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                Some(current),
                false,
            ));
        }
        let lease = ExecutionLeaseRecord {
            expires_at: request.expires_at.clone(),
            ..current
        };
        let response = lease_receipt(LeaseWriteStatus::Accepted, Some(lease.clone()), false);
        transaction
            .execute(
                "UPDATE execution_leases SET expires_at = ?1
                 WHERE job_id = ?2 AND lease_id = ?3 AND attempt = ?4 AND fencing_token = ?5",
                params![
                    request.expires_at.0,
                    request.job_id.0,
                    request.lease_id.0,
                    i64::try_from(request.attempt)
                        .map_err(|_| StorageError::invalid_input("attempt is out of range"))?,
                    request.fencing_token.0,
                ],
            )
            .map_err(sql_error)?;
        insert_lease_request_receipt(
            &transaction,
            "renew",
            &request.job_id,
            &request.request_id,
            &request_digest,
            &response,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(response)
    }

    /// Records one dispatch result after joining every lease identity to the
    /// current durable Worker and lease rows.
    ///
    /// The accepted receipt is the only new row this operation creates.  A
    /// duplicate or changed-body replay is decided from that receipt before
    /// reading current authority, so a restart or lease replacement cannot
    /// turn an accepted result into a second state transition.
    ///
    /// # Errors
    ///
    /// Returns an input error for malformed identities or an adapter error
    /// for a failed `SQLite` read/write.
    #[allow(clippy::too_many_lines)]
    pub fn record_dispatch_result(
        &mut self,
        request: &DispatchResultRequest,
    ) -> Result<DispatchResultReceipt, StorageError> {
        let request_digest = dispatch_result_digest(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;

        if let Some(receipt) = dispatch_result_replay(
            &transaction,
            &request.job_id,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        validate_dispatch_result(request)?;

        let Some(worker) = load_worker_in_transaction(&transaction, &request.worker_id)? else {
            transaction.commit().map_err(sql_error)?;
            return Ok(dispatch_result_rejection(
                DispatchResultStatus::RejectedWorkerInstance,
                DispatchResultErrorCode::WorkerNotRegistered,
            ));
        };
        if worker.worker_instance_id != request.worker_instance_id {
            transaction.commit().map_err(sql_error)?;
            return Ok(dispatch_result_rejection(
                DispatchResultStatus::RejectedWorkerInstance,
                DispatchResultErrorCode::WorkerInstanceChanged,
            ));
        }

        let Some(current) = load_lease_in_transaction(&transaction, &request.job_id)? else {
            transaction.commit().map_err(sql_error)?;
            return Ok(dispatch_result_rejection(
                DispatchResultStatus::Conflict,
                DispatchResultErrorCode::JobDispatchConflict,
            ));
        };
        if less_decimal(&request.fencing_token.0, &current.fencing_token.0) {
            transaction.commit().map_err(sql_error)?;
            return Ok(dispatch_result_rejection(
                DispatchResultStatus::RejectedStaleFencingToken,
                DispatchResultErrorCode::StaleFencingToken,
            ));
        }
        if request.checked_at.0 >= current.expires_at.0 || request.sent_at.0 >= current.expires_at.0
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(dispatch_result_rejection(
                DispatchResultStatus::RejectedExpiredLease,
                DispatchResultErrorCode::LeaseExpired,
            ));
        }
        if request.sent_at.0 < current.issued_at.0 {
            transaction.commit().map_err(sql_error)?;
            return Ok(dispatch_result_rejection(
                DispatchResultStatus::Conflict,
                DispatchResultErrorCode::JobDispatchConflict,
            ));
        }
        if request.payload_digest != current.payload_digest {
            transaction.commit().map_err(sql_error)?;
            return Ok(dispatch_result_rejection(
                DispatchResultStatus::Conflict,
                DispatchResultErrorCode::JobDispatchConflict,
            ));
        }
        if request.lease_id != current.lease_id
            || request.worker_id != current.worker_id
            || request.worker_instance_id != current.worker_instance_id
            || request.attempt != current.attempt
            || request.fencing_token != current.fencing_token
            || request.issued_at != current.issued_at
            || request.expires_at != current.expires_at
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(dispatch_result_rejection(
                DispatchResultStatus::Conflict,
                DispatchResultErrorCode::JobDispatchConflict,
            ));
        }

        let response = DispatchResultReceipt {
            status: request.status,
            error: None,
            replayed: false,
        };
        insert_dispatch_result_receipt(
            &transaction,
            &request.job_id,
            &request.request_id,
            &request_digest,
            response,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(response)
    }

    /// Loads the current durable Worker record.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the `SQLite` read fails.
    pub fn load_worker(&self, worker_id: &WorkerId) -> Result<Option<WorkerRecord>, StorageError> {
        validate_id(&worker_id.0, "wrk_", "workerId")?;
        load_worker_in_transaction(self.storage.connection()?, worker_id)
    }

    /// Loads the current durable Job lease.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the `SQLite` read fails.
    pub fn load_lease(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionLeaseRecord>, StorageError> {
        validate_id(&job_id.0, "job_", "jobId")?;
        load_lease_in_transaction(self.storage.connection()?, job_id)
    }

    /// Reports whether one exact lease request receipt exists.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the `SQLite` read fails.
    pub fn has_request(
        &self,
        operation: &str,
        job_id: &ExecutionJobId,
        request_id: &RequestId,
    ) -> Result<bool, StorageError> {
        if !matches!(operation, "claim" | "renew" | "dispatch_result") {
            return Err(StorageError::invalid_input(
                "execution lease operation is invalid",
            ));
        }
        validate_id(&job_id.0, "job_", "jobId")?;
        validate_id(&request_id.0, "req_", "requestId")?;
        Ok(self
            .storage
            .connection()?
            .query_row(
                "SELECT 1 FROM execution_lease_request_receipts
                 WHERE operation = ?1 AND job_id = ?2 AND request_id = ?3",
                params![operation, job_id.0, request_id.0],
                |_| Ok(()),
            )
            .optional()
            .map_err(sql_error)?
            .is_some())
    }

    fn ensure_schema(&self) -> Result<(), StorageError> {
        self.storage
            .connection()?
            .execute_batch(EXECUTION_REGISTRY_SCHEMA)
            .map_err(sql_error)
    }
}

fn validate_registration(request: &WorkerRegistrationRequest) -> Result<(), StorageError> {
    validate_id(&request.worker_id.0, "wrk_", "workerId")?;
    validate_id(&request.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_id(&request.message_id.0, "xmsg_", "messageId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_instant(&request.sent_at, "sentAt")?;
    validate_instant(&request.started_at, "startedAt")?;
    if request.sent_at.0 < request.started_at.0 {
        return Err(StorageError::invalid_input("sentAt precedes startedAt"));
    }
    validate_capabilities(&request.capabilities)
}

fn validate_heartbeat(request: &WorkerHeartbeatRequest) -> Result<(), StorageError> {
    validate_id(&request.worker_id.0, "wrk_", "workerId")?;
    validate_id(&request.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_id(&request.message_id.0, "xmsg_", "messageId")?;
    validate_instant(&request.sent_at, "sentAt")?;
    validate_instant(&request.observed_at, "observedAt")?;
    if request.sent_at.0 > request.observed_at.0 {
        return Err(StorageError::invalid_input("sentAt follows observedAt"));
    }
    let _ = sequence_value(&request.heartbeat_sequence, "heartbeat sequence")?;
    if request.max_slots > MAX_WORKER_SLOTS || request.available_slots > MAX_WORKER_SLOTS {
        return Err(StorageError::invalid_input(
            "worker slots exceed the maximum",
        ));
    }
    if request.available_slots > request.max_slots {
        return Err(StorageError::invalid_input(
            "available slots exceed max slots",
        ));
    }
    if request.active_leases.len() > MAX_ACTIVE_LEASES {
        return Err(StorageError::invalid_input(
            "active leases exceed the maximum count",
        ));
    }
    let mut jobs = HashSet::new();
    let mut leases = HashSet::new();
    for lease in &request.active_leases {
        validate_id(&lease.job_id.0, "job_", "activeLeases.jobId")?;
        validate_id(&lease.lease_id.0, "lse_", "activeLeases.leaseId")?;
        validate_fencing_token(&lease.fencing_token)?;
        if lease.attempt == 0
            || lease.attempt > MAX_LEASE_ATTEMPT
            || !jobs.insert(&lease.job_id.0)
            || !leases.insert(&lease.lease_id.0)
        {
            return Err(StorageError::invalid_input(
                "active lease summary is not unique",
            ));
        }
    }
    Ok(())
}

fn validate_claim(request: &ExecutionLeaseClaim) -> Result<(), StorageError> {
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.lease_id.0, "lse_", "leaseId")?;
    validate_id(&request.message_id.0, "xmsg_", "messageId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_id(&request.worker_id.0, "wrk_", "workerId")?;
    validate_id(&request.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_digest(&request.payload_digest)?;
    validate_fencing_token(&request.fencing_token)?;
    if request.attempt == 0 || request.attempt > MAX_LEASE_ATTEMPT {
        return Err(StorageError::invalid_input(
            "attempt is outside the supported range",
        ));
    }
    validate_lease_window(&request.issued_at, &request.expires_at)
}

fn validate_renewal(request: &ExecutionLeaseRenewal) -> Result<(), StorageError> {
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.lease_id.0, "lse_", "leaseId")?;
    validate_id(&request.message_id.0, "xmsg_", "messageId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_id(&request.worker_id.0, "wrk_", "workerId")?;
    validate_id(&request.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_fencing_token(&request.fencing_token)?;
    if request.attempt == 0 || request.attempt > MAX_LEASE_ATTEMPT {
        return Err(StorageError::invalid_input(
            "attempt is outside the supported range",
        ));
    }
    validate_instant(&request.sent_at, "sentAt")?;
    validate_lease_window(&request.prior_expires_at, &request.expires_at)
}

fn validate_dispatch_result(request: &DispatchResultRequest) -> Result<(), StorageError> {
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.lease_id.0, "lse_", "leaseId")?;
    validate_id(&request.message_id.0, "xmsg_", "messageId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_id(&request.worker_id.0, "wrk_", "workerId")?;
    validate_id(&request.worker_instance_id.0, "wki_", "workerInstanceId")?;
    if let Some(worker_session_id) = request.worker_session_id.as_ref() {
        validate_id(&worker_session_id.0, "wsn_", "workerSessionId")?;
    }
    validate_digest(&request.payload_digest)?;
    validate_fencing_token(&request.fencing_token)?;
    if request.attempt == 0 {
        return Err(StorageError::invalid_input("attempt must be positive"));
    }
    validate_instant(&request.checked_at, "checkedAt")?;
    validate_lease_window(&request.issued_at, &request.expires_at)
}

fn validate_lease_window(issued_at: &Instant, expires_at: &Instant) -> Result<(), StorageError> {
    validate_instant(issued_at, "issuedAt")?;
    validate_instant(expires_at, "expiresAt")?;
    if issued_at.0 >= expires_at.0 {
        return Err(StorageError::invalid_input(
            "lease expiresAt must follow issuedAt",
        ));
    }
    Ok(())
}

fn validate_capabilities(capabilities: &[String]) -> Result<(), StorageError> {
    if capabilities.is_empty() {
        return Err(StorageError::invalid_input(
            "capabilities must contain at least one value",
        ));
    }
    if capabilities.len() > 128 {
        return Err(StorageError::invalid_input(
            "capabilities exceed the maximum count",
        ));
    }
    for capability in capabilities {
        if capability.is_empty() || capability.len() > 200 || !capability.is_ascii() {
            return Err(StorageError::invalid_input("capability value is invalid"));
        }
    }
    Ok(())
}

fn validate_id(value: &str, prefix: &str, field: &str) -> Result<(), StorageError> {
    let valid = value
        .strip_prefix(prefix)
        .is_some_and(|suffix| suffix.len() == 26 && suffix.bytes().all(crockford_byte));
    if !valid {
        return Err(StorageError::invalid_input(format!(
            "{field} is not canonical"
        )));
    }
    Ok(())
}

fn crockford_byte(byte: u8) -> bool {
    byte.is_ascii_digit()
        || matches!(
            byte,
            b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
        )
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), StorageError> {
    let Some(value) = digest.0.strip_prefix("sha256:") else {
        return Err(StorageError::invalid_input(
            "payload digest is not canonical",
        ));
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(StorageError::invalid_input(
            "payload digest is not canonical",
        ));
    }
    Ok(())
}

fn validate_fencing_token(token: &FencingToken) -> Result<(), StorageError> {
    if token.0.is_empty()
        || token.0.len() > 20
        || token.0.starts_with('0')
        || !token.0.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(StorageError::invalid_input(
            "fencing token is not canonical",
        ));
    }
    Ok(())
}

fn validate_instant(value: &Instant, field: &str) -> Result<(), StorageError> {
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
        Err(StorageError::invalid_input(format!(
            "{field} is not canonical"
        )))
    }
}

fn sequence_value(value: &ExecutionSequence, field: &str) -> Result<u64, StorageError> {
    let value = u64::try_from(value.0)
        .map_err(|_| StorageError::invalid_input(format!("{field} is invalid")))?;
    if value == 0 || value > MAX_EXECUTION_SEQUENCE {
        return Err(StorageError::invalid_input(format!("{field} is invalid")));
    }
    Ok(value)
}

fn digest<T: Serialize>(value: &T) -> Result<String, StorageError> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode execution registry request: {error}"
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn encode_json<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| {
        StorageError::adapter(format!(
            "failed to encode execution registry value: {error}"
        ))
    })
}

fn decode_json<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|error| {
        StorageError::adapter(format!("execution registry value is corrupt: {error}"))
    })
}

fn empty_worker_record(request: &WorkerRegistrationRequest) -> WorkerRecord {
    WorkerRecord {
        worker_id: request.worker_id.clone(),
        worker_instance_id: request.worker_instance_id.clone(),
        started_at: request.started_at.clone(),
        capabilities: request.capabilities.clone(),
        last_heartbeat_at: None,
        heartbeat_sequence: 0,
        max_slots: 0,
        available_slots: 0,
    }
}

fn validate_worker_record(worker: &WorkerRecord) -> Result<(), StorageError> {
    validate_id(&worker.worker_id.0, "wrk_", "workerId")?;
    validate_id(&worker.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_instant(&worker.started_at, "startedAt")?;
    if let Some(last_heartbeat_at) = &worker.last_heartbeat_at {
        validate_instant(last_heartbeat_at, "lastHeartbeatAt")?;
    }
    validate_capabilities(&worker.capabilities)?;
    if worker.heartbeat_sequence > MAX_EXECUTION_SEQUENCE {
        return Err(StorageError::invalid_input(
            "heartbeat sequence is out of range",
        ));
    }
    if worker.max_slots > MAX_WORKER_SLOTS
        || worker.available_slots > MAX_WORKER_SLOTS
        || worker.available_slots > worker.max_slots
    {
        return Err(StorageError::invalid_input("worker slots are invalid"));
    }
    Ok(())
}

fn validate_lease_record(lease: &ExecutionLeaseRecord) -> Result<(), StorageError> {
    validate_id(&lease.job_id.0, "job_", "jobId")?;
    validate_id(&lease.lease_id.0, "lse_", "leaseId")?;
    validate_id(&lease.worker_id.0, "wrk_", "workerId")?;
    validate_id(&lease.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_digest(&lease.payload_digest)?;
    validate_fencing_token(&lease.fencing_token)?;
    if lease.attempt == 0 || lease.attempt > MAX_LEASE_ATTEMPT {
        return Err(StorageError::invalid_input(
            "attempt is outside the supported range",
        ));
    }
    validate_lease_window(&lease.issued_at, &lease.expires_at)
}

fn validate_stored_worker(worker: &WorkerRecord) -> Result<(), StorageError> {
    validate_worker_record(worker).map_err(|error| {
        StorageError::adapter(format!("durable Worker record is corrupt: {error}"))
    })
}

fn validate_stored_lease(lease: &ExecutionLeaseRecord) -> Result<(), StorageError> {
    validate_lease_record(lease).map_err(|error| {
        StorageError::adapter(format!("durable execution lease is corrupt: {error}"))
    })
}

fn load_worker_in_transaction(
    connection: &rusqlite::Connection,
    worker_id: &WorkerId,
) -> Result<Option<WorkerRecord>, StorageError> {
    let worker = connection
        .query_row(
            "SELECT worker_id, worker_instance_id, started_at, capabilities,
                    last_heartbeat_at, heartbeat_sequence, max_slots, available_slots
             FROM execution_workers WHERE worker_id = ?1",
            params![worker_id.0],
            |row| {
                let heartbeat_sequence = row.get::<_, i64>(5)?;
                let max_slots = row.get::<_, i64>(6)?;
                let available_slots = row.get::<_, i64>(7)?;
                Ok(WorkerRecord {
                    worker_id: WorkerId(row.get(0)?),
                    worker_instance_id: WorkerInstanceId(row.get(1)?),
                    started_at: Instant(row.get(2)?),
                    capabilities: decode_json_row(&row.get::<_, String>(3)?)?,
                    last_heartbeat_at: row.get::<_, Option<String>>(4)?.map(Instant),
                    heartbeat_sequence: u64::try_from(heartbeat_sequence).map_err(|_| {
                        rusqlite::Error::IntegralValueOutOfRange(0, heartbeat_sequence)
                    })?,
                    max_slots: u64::try_from(max_slots)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, max_slots))?,
                    available_slots: u64::try_from(available_slots).map_err(|_| {
                        rusqlite::Error::IntegralValueOutOfRange(0, available_slots)
                    })?,
                })
            },
        )
        .optional()
        .map_err(sql_error)?;
    if let Some(worker) = worker.as_ref() {
        validate_stored_worker(worker)?;
    }
    Ok(worker)
}

fn decode_json_row<T: for<'de> Deserialize<'de>>(value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            value.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn load_lease_in_transaction(
    connection: &rusqlite::Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionLeaseRecord>, StorageError> {
    let lease = connection
        .query_row(
            "SELECT job_id, lease_id, payload_digest, worker_id, worker_instance_id,
                    attempt, fencing_token, issued_at, expires_at
             FROM execution_leases WHERE job_id = ?1",
            params![job_id.0],
            |row| {
                let attempt = row.get::<_, i64>(5)?;
                Ok(ExecutionLeaseRecord {
                    job_id: ExecutionJobId(row.get(0)?),
                    lease_id: LeaseId(row.get(1)?),
                    payload_digest: Sha256Digest(row.get(2)?),
                    worker_id: WorkerId(row.get(3)?),
                    worker_instance_id: WorkerInstanceId(row.get(4)?),
                    attempt: u64::try_from(attempt)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, attempt))?,
                    fencing_token: FencingToken(row.get(6)?),
                    issued_at: Instant(row.get(7)?),
                    expires_at: Instant(row.get(8)?),
                })
            },
        )
        .optional()
        .map_err(sql_error)?;
    if let Some(lease) = lease.as_ref() {
        validate_stored_lease(lease)?;
    }
    Ok(lease)
}

fn has_old_worker_leases(
    connection: &rusqlite::Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<bool, StorageError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM execution_leases
             WHERE worker_id = ?1 AND worker_instance_id != ?2 LIMIT 1",
            params![worker_id.0, worker_instance_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some())
}

fn heartbeat_lease_status(
    connection: &rusqlite::Connection,
    request: &WorkerHeartbeatRequest,
) -> Result<Option<LeaseWriteStatus>, StorageError> {
    for summary in &request.active_leases {
        let Some(current) = load_lease_in_transaction(connection, &summary.job_id)? else {
            return Ok(Some(LeaseWriteStatus::RejectedConflict));
        };
        if current.worker_id != request.worker_id
            || current.worker_instance_id != request.worker_instance_id
        {
            return Ok(Some(LeaseWriteStatus::RejectedWorkerInstance));
        }
        if less_decimal(&summary.fencing_token.0, &current.fencing_token.0) {
            return Ok(Some(LeaseWriteStatus::RejectedStaleFencingToken));
        }
        if summary.lease_id != current.lease_id || summary.attempt != current.attempt {
            return Ok(Some(LeaseWriteStatus::RejectedConflict));
        }
        if request.observed_at.0 >= current.expires_at.0 {
            return Ok(Some(LeaseWriteStatus::RejectedExpiredLease));
        }
    }
    Ok(None)
}

fn worker_instance_seen(
    connection: &rusqlite::Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<bool, StorageError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM execution_worker_instances
             WHERE worker_id = ?1 AND worker_instance_id = ?2",
            params![worker_id.0, worker_instance_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some())
}

fn worker_instance_is_current(
    connection: &rusqlite::Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<bool, StorageError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM execution_workers
             WHERE worker_id = ?1 AND worker_instance_id = ?2",
            params![worker_id.0, worker_instance_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some())
}

fn dispatch_result_digest(request: &DispatchResultRequest) -> Result<String, StorageError> {
    #[derive(Serialize)]
    struct DigestInput<'a> {
        expires_at: &'a Instant,
        fencing_token: &'a FencingToken,
        issued_at: &'a Instant,
        job_id: &'a ExecutionJobId,
        lease_id: &'a LeaseId,
        message_id: &'a ExecutionMessageId,
        payload_digest: &'a Sha256Digest,
        request_id: &'a RequestId,
        sent_at: &'a Instant,
        status: DispatchResultStatus,
        attempt: u64,
        error: &'a Option<String>,
        worker_id: &'a WorkerId,
        worker_instance_id: &'a WorkerInstanceId,
        worker_session_id: &'a Option<WorkerSessionId>,
    }

    digest(&DigestInput {
        expires_at: &request.expires_at,
        fencing_token: &request.fencing_token,
        issued_at: &request.issued_at,
        job_id: &request.job_id,
        lease_id: &request.lease_id,
        message_id: &request.message_id,
        payload_digest: &request.payload_digest,
        request_id: &request.request_id,
        sent_at: &request.sent_at,
        status: request.status,
        attempt: request.attempt,
        error: &request.error,
        worker_id: &request.worker_id,
        worker_instance_id: &request.worker_instance_id,
        worker_session_id: &request.worker_session_id,
    })
}

fn dispatch_result_replay(
    connection: &rusqlite::Connection,
    job_id: &ExecutionJobId,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<Option<DispatchResultReceipt>, StorageError> {
    let Some((stored_digest, response_json)) = connection
        .query_row(
            "SELECT request_digest, response_json
             FROM execution_lease_request_receipts
             WHERE operation = 'dispatch_result' AND job_id = ?1 AND request_id = ?2",
            params![job_id.0, request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?
    else {
        return Ok(None);
    };
    if stored_digest != request_digest {
        return Ok(Some(dispatch_result_rejection(
            DispatchResultStatus::Conflict,
            DispatchResultErrorCode::MessageConflict,
        )));
    }
    let mut response = decode_json::<DispatchResultReceipt>(&response_json)?;
    response.status = DispatchResultStatus::Duplicate;
    response.error = None;
    response.replayed = true;
    Ok(Some(response))
}

fn insert_dispatch_result_receipt(
    connection: &rusqlite::Transaction<'_>,
    job_id: &ExecutionJobId,
    request_id: &RequestId,
    request_digest: &str,
    response: DispatchResultReceipt,
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO execution_lease_request_receipts
                (operation, job_id, request_id, request_digest, response_json)
             VALUES ('dispatch_result', ?1, ?2, ?3, ?4)",
            params![
                job_id.0,
                request_id.0,
                request_digest,
                encode_json(&response)?
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn dispatch_result_rejection(
    status: DispatchResultStatus,
    code: DispatchResultErrorCode,
) -> DispatchResultReceipt {
    DispatchResultReceipt {
        status,
        error: Some(DispatchResultError {
            code,
            retryable: false,
        }),
        replayed: false,
    }
}

fn lease_request_replay(
    connection: &rusqlite::Connection,
    operation: &str,
    job_id: &ExecutionJobId,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<Option<ExecutionLeaseReceipt>, StorageError> {
    let Some((stored_digest, response_json)) = connection
        .query_row(
            "SELECT request_digest, response_json
             FROM execution_lease_request_receipts
             WHERE operation = ?1 AND job_id = ?2 AND request_id = ?3",
            params![operation, job_id.0, request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?
    else {
        return Ok(None);
    };
    if stored_digest != request_digest {
        return Ok(Some(lease_receipt(
            LeaseWriteStatus::RejectedConflict,
            None,
            false,
        )));
    }
    let mut response = decode_json::<ExecutionLeaseReceipt>(&response_json)?;
    if let Some(lease) = response.lease.as_ref() {
        validate_stored_lease(lease)?;
    }
    response.status = LeaseWriteStatus::Duplicate;
    response.replayed = true;
    Ok(Some(response))
}

fn insert_lease_request_receipt(
    connection: &rusqlite::Transaction<'_>,
    operation: &str,
    job_id: &ExecutionJobId,
    request_id: &RequestId,
    request_digest: &str,
    response: &ExecutionLeaseReceipt,
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO execution_lease_request_receipts
                (operation, job_id, request_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                operation,
                job_id.0,
                request_id.0,
                request_digest,
                encode_json(response)?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn lease_receipt(
    status: LeaseWriteStatus,
    lease: Option<ExecutionLeaseRecord>,
    replayed: bool,
) -> ExecutionLeaseReceipt {
    ExecutionLeaseReceipt {
        status,
        lease,
        replayed,
    }
}

fn less_decimal(left: &str, right: &str) -> bool {
    left.len() < right.len() || (left.len() == right.len() && left < right)
}

#[allow(clippy::needless_pass_by_value)]
fn sql_error(error: rusqlite::Error) -> StorageError {
    StorageError::adapter(format!("execution registry SQLite error: {error}"))
}
