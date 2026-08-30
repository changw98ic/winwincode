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

use crate::{
    EXECUTION_PROTOCOL_VERSION, NewOutboxEvent, ProductStateStorage, ProjectionEventStream,
    PublicEventScope, ReceiptIdentity, SqliteStorage, StateCommit, StorageError,
    WorkerAuthenticationIdentity, WorkerCapacityEntry, WorkerCapacitySnapshot, WorkerHealth,
    WorkerManagementPage, WorkerManagementPageCursor, WorkerManagementSnapshot,
    WorkerManagementState, WorkerOperationalState, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationErrorCode, WorkerRegistryScope, receipt_scope_key,
};

const EXECUTION_REGISTRY_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS execution_workers (
    worker_id TEXT PRIMARY KEY NOT NULL,
    worker_instance_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    authentication_identity TEXT NOT NULL,
    protocol_version TEXT NOT NULL,
    platform TEXT NOT NULL,
    capabilities TEXT NOT NULL,
    capability_digest TEXT NOT NULL,
    security_zone TEXT NOT NULL,
    health TEXT NOT NULL,
    last_heartbeat_at TEXT,
    heartbeat_sequence INTEGER NOT NULL CHECK (heartbeat_sequence >= 0),
    max_slots INTEGER NOT NULL CHECK (max_slots > 0),
    running_slots INTEGER NOT NULL CHECK (running_slots >= 0),
    available_slots INTEGER NOT NULL CHECK (available_slots >= 0),
    CHECK (running_slots + available_slots = max_slots)
);
CREATE TABLE IF NOT EXISTS execution_worker_instances (
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    PRIMARY KEY (worker_id, worker_instance_id)
);
CREATE TABLE IF NOT EXISTS execution_worker_scopes (
    worker_id TEXT PRIMARY KEY NOT NULL,
    scope_key TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    FOREIGN KEY (worker_id) REFERENCES execution_workers(worker_id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS execution_worker_registration_receipts (
    worker_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (worker_id, request_id)
);
CREATE TABLE IF NOT EXISTS execution_worker_authenticated_placements (
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    worker_pool_id TEXT NOT NULL,
    registration_request_id TEXT NOT NULL,
    placement_digest TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY (worker_id, worker_instance_id),
    UNIQUE (registration_request_id),
    FOREIGN KEY (worker_id) REFERENCES execution_workers(worker_id) ON DELETE CASCADE
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
CREATE TABLE IF NOT EXISTS execution_dispatch_authorities (
    job_id TEXT PRIMARY KEY NOT NULL,
    lease_id TEXT UNIQUE NOT NULL,
    payload_digest TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    worker_session_id TEXT UNIQUE NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0),
    fencing_token TEXT NOT NULL,
    issued_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    dispatch_request_id TEXT UNIQUE NOT NULL,
    accepted_at TEXT NOT NULL,
    FOREIGN KEY (job_id) REFERENCES execution_leases(job_id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS execution_lease_terminals (
    lease_id TEXT PRIMARY KEY NOT NULL,
    job_id TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0),
    fencing_token TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('completed', 'cancelled', 'failed')),
    terminal_at TEXT NOT NULL,
    request_id TEXT UNIQUE NOT NULL,
    request_digest TEXT NOT NULL,
    UNIQUE (job_id, attempt),
    FOREIGN KEY (job_id) REFERENCES execution_leases(job_id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS execution_lease_authenticated_placements (
    job_id TEXT PRIMARY KEY NOT NULL,
    lease_id TEXT UNIQUE NOT NULL,
    placement_digest TEXT NOT NULL,
    record_json TEXT NOT NULL,
    FOREIGN KEY (job_id) REFERENCES execution_leases(job_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS execution_leases_worker_instance
    ON execution_leases (worker_id, worker_instance_id);
";

const MAX_EXECUTION_SEQUENCE: u64 = 9_007_199_254_740_991;
const MAX_LEASE_ATTEMPT: u64 = 1_000;
const MAX_ACTIVE_LEASES: usize = 1_024;
const MAX_WORKER_SLOTS: u64 = 1_024;
const MAX_WORKER_PAGE_SIZE: usize = 200;
const WORKER_MANAGEMENT_STREAM_PREFIX: &str = "worker-management:";
const WORKER_MANAGEMENT_RECEIPT_TOPIC: &str = "worker.management.receipt.v1";

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
    pub management_scope: WorkerRegistryScope,
    pub worker_instance_id: WorkerInstanceId,
    pub started_at: Instant,
    pub authentication_identity: WorkerAuthenticationIdentity,
    pub protocol_version: String,
    pub platform: WorkerPlatform,
    pub capabilities: Vec<String>,
    pub capability_digest: Sha256Digest,
    pub security_zone: String,
    pub health: WorkerHealth,
    pub last_heartbeat_at: Option<Instant>,
    pub heartbeat_sequence: u64,
    pub max_slots: u64,
    pub running_slots: u64,
    pub available_slots: u64,
}

/// Authenticator-sealed pool placement for one exact Worker process.
///
/// This record is attribution authority only. Pool capacity and session slots
/// remain owned by their existing scheduler and slot stores.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AuthenticatedWorkerPlacement {
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_pool_id: WorkerPoolId,
    pub management_scope: WorkerRegistryScope,
    pub authentication_identity: WorkerAuthenticationIdentity,
    pub registration_request_id: RequestId,
    pub placed_at: Instant,
}

/// Immutable pool attribution copied beside one accepted Registry lease.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionLeasePlacement {
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_pool_id: WorkerPoolId,
    pub worker_placement_digest: Sha256Digest,
    pub claimed_at: Instant,
}

/// Replay-safe operator mutation committed beside its public invalidation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerManagementCommand {
    pub receipt_identity: ReceiptIdentity,
    pub command_digest: Sha256Digest,
    pub scope: WorkerRegistryScope,
    pub worker_id: WorkerId,
    pub expected_revision: u64,
    pub target_state: WorkerManagementState,
    pub occurred_at: Instant,
    pub public_event: NewOutboxEvent,
}

/// Exact durable result of one Worker management command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerManagementReceipt {
    pub previous_revision: u64,
    pub worker: WorkerManagementSnapshot,
    pub replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerManagementRecord {
    scope: WorkerRegistryScope,
    worker_id: WorkerId,
    state: WorkerManagementState,
    revision: u64,
    updated_at: Instant,
}

/// Worker registration input independent of generated wire DTOs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerRegistrationRequest {
    pub authentication_identity: WorkerAuthenticationIdentity,
    pub protocol_version: String,
    pub platform: WorkerPlatform,
    pub capabilities: Vec<String>,
    pub capability_digest: Sha256Digest,
    pub security_zone: String,
    pub max_slots: u64,
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
    pub error: Option<WorkerRegistrationErrorCode>,
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
    pub running_slots: u64,
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

/// Immutable authority retained only after the Registry accepted an exact
/// Worker dispatch result under the current lease.
///
/// Fields stay private so a downstream adapter cannot turn an untrusted
/// `session.binding`, runtime event, or outcome frame into scheduler authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionDispatchAuthority {
    lease: ExecutionLeaseRecord,
    worker_session_id: WorkerSessionId,
    dispatch_request_id: RequestId,
    accepted_at: Instant,
}

impl ExecutionDispatchAuthority {
    #[must_use]
    pub const fn lease(&self) -> &ExecutionLeaseRecord {
        &self.lease
    }

    #[must_use]
    pub const fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }

    #[must_use]
    pub const fn dispatch_request_id(&self) -> &RequestId {
        &self.dispatch_request_id
    }

    #[must_use]
    pub const fn accepted_at(&self) -> &Instant {
        &self.accepted_at
    }
}

/// Terminal outcome which removes an immutable lease from active capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLeaseTerminalOutcome {
    Completed,
    Cancelled,
    Failed,
}

impl ExecutionLeaseTerminalOutcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// Exact fenced authority for terminalizing one immutable execution lease.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionLeaseTerminalRequest {
    pub job_id: ExecutionJobId,
    pub lease_id: LeaseId,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub attempt: u64,
    pub fencing_token: FencingToken,
    pub outcome: ExecutionLeaseTerminalOutcome,
    pub terminal_at: Instant,
    pub request_id: RequestId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutionLeaseTerminalRecord {
    outcome: ExecutionLeaseTerminalOutcome,
    terminal_at: Instant,
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

    /// Registers one embedded Community Worker in the canonical local scope.
    ///
    /// Fleet and other remotely scoped adapters must call
    /// [`Self::register_worker_for_scope`] with their authenticated scope.
    ///
    /// # Errors
    ///
    /// Returns an input error for a malformed request or an adapter error for
    /// a failed `SQLite` read/write.
    pub fn register_worker(
        &mut self,
        request: &WorkerRegistrationRequest,
    ) -> Result<WorkerRegistrationReceipt, StorageError> {
        self.register_worker_for_scope(request, &WorkerRegistryScope::local_default())
    }

    /// Registers one Worker in an exact tenant scope using the same registry
    /// transaction as identity and instance registration.
    ///
    /// # Errors
    ///
    /// Returns an input error for a malformed request/scope or an adapter
    /// error for a failed `SQLite` read/write.
    #[allow(clippy::too_many_lines)]
    pub fn register_worker_for_scope(
        &mut self,
        request: &WorkerRegistrationRequest,
        scope: &WorkerRegistryScope,
    ) -> Result<WorkerRegistrationReceipt, StorageError> {
        validate_registration(request)?;
        validate_worker_scope(scope)?;
        if let Some(error) = registration_profile_rejection(request) {
            return Ok(registration_rejection(
                empty_worker_record(request, scope),
                error,
            ));
        }
        let request_digest = registration_digest(request, scope)?;
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
                    .unwrap_or_else(|| empty_worker_record(request, scope));
                let error = registration_conflict_code(&worker, request, scope);
                transaction.commit().map_err(sql_error)?;
                return Ok(registration_rejection(worker, error));
            }
            let mut response = decode_json::<WorkerRegistrationReceipt>(&response_json)?;
            validate_stored_worker(&response.worker)?;
            response.status = WorkerRegistrationStatus::Duplicate;
            transaction.commit().map_err(sql_error)?;
            return Ok(response);
        }

        let prior = load_worker_in_transaction(&transaction, &request.worker_id)?;
        if let Some(existing) = prior.as_ref()
            && &existing.management_scope != scope
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(registration_rejection(
                existing.clone(),
                WorkerRegistrationErrorCode::ScopeMismatch,
            ));
        }
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
                error: Some(WorkerRegistrationErrorCode::MessageConflict),
                lease_recovery: LeaseRecovery::NoActiveLeases,
                worker: prior.unwrap_or_else(|| empty_worker_record(request, scope)),
            });
        }
        if let Some(existing) = prior.as_ref()
            && existing.worker_instance_id == request.worker_instance_id
            && existing.started_at != request.started_at
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerRegistrationReceipt {
                status: WorkerRegistrationStatus::RejectedConflict,
                error: Some(WorkerRegistrationErrorCode::MessageConflict),
                lease_recovery: LeaseRecovery::NoActiveLeases,
                worker: existing.clone(),
            });
        }
        if let Some(existing) = prior.as_ref()
            && existing.worker_instance_id == request.worker_instance_id
            && let Some(error) = registration_profile_conflict(existing, request, scope)
        {
            transaction.commit().map_err(sql_error)?;
            return Ok(registration_rejection(existing.clone(), error));
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
            management_scope: scope.clone(),
            worker_instance_id: request.worker_instance_id.clone(),
            started_at: request.started_at.clone(),
            authentication_identity: request.authentication_identity.clone(),
            protocol_version: request.protocol_version.clone(),
            platform: request.platform,
            capabilities: request.capabilities.clone(),
            capability_digest: request.capability_digest.clone(),
            security_zone: request.security_zone.clone(),
            health: prior
                .as_ref()
                .filter(|worker| worker.worker_instance_id == request.worker_instance_id)
                .map_or(WorkerHealth::Registered, |worker| worker.health),
            last_heartbeat_at,
            heartbeat_sequence,
            max_slots: prior
                .as_ref()
                .filter(|worker| worker.worker_instance_id == request.worker_instance_id)
                .map_or(request.max_slots, |worker| worker.max_slots),
            running_slots: prior
                .as_ref()
                .filter(|worker| worker.worker_instance_id == request.worker_instance_id)
                .map_or(0, |worker| worker.running_slots),
            available_slots: prior
                .as_ref()
                .filter(|worker| worker.worker_instance_id == request.worker_instance_id)
                .map_or(request.max_slots, |worker| worker.available_slots),
        };
        let response = WorkerRegistrationReceipt {
            status: WorkerRegistrationStatus::Accepted,
            error: None,
            lease_recovery,
            worker: worker.clone(),
        };
        let authentication_identity = encode_json(&worker.authentication_identity)?;
        let capabilities = encode_json(&worker.capabilities)?;
        let last_heartbeat_at = worker
            .last_heartbeat_at
            .as_ref()
            .map(|value| value.0.as_str());
        transaction
            .execute(
                "INSERT INTO execution_workers
                    (worker_id, worker_instance_id, started_at,
                     authentication_identity, protocol_version, platform, capabilities,
                     capability_digest, security_zone, health, last_heartbeat_at,
                     heartbeat_sequence, max_slots, running_slots, available_slots)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(worker_id) DO UPDATE SET
                    worker_instance_id = excluded.worker_instance_id,
                    started_at = excluded.started_at,
                    authentication_identity = excluded.authentication_identity,
                    protocol_version = excluded.protocol_version,
                    platform = excluded.platform,
                    capabilities = excluded.capabilities,
                    capability_digest = excluded.capability_digest,
                    security_zone = excluded.security_zone,
                    health = excluded.health,
                    last_heartbeat_at = excluded.last_heartbeat_at,
                    heartbeat_sequence = excluded.heartbeat_sequence,
                    max_slots = excluded.max_slots,
                    running_slots = excluded.running_slots,
                    available_slots = excluded.available_slots",
                params![
                    worker.worker_id.0,
                    worker.worker_instance_id.0,
                    worker.started_at.0,
                    authentication_identity,
                    worker.protocol_version,
                    worker.platform.as_str(),
                    capabilities,
                    worker.capability_digest.0,
                    worker.security_zone,
                    worker.health.as_str(),
                    last_heartbeat_at,
                    i64::try_from(worker.heartbeat_sequence).map_err(|_| {
                        StorageError::invalid_input("heartbeat sequence is out of range")
                    })?,
                    i64::try_from(worker.max_slots)
                        .map_err(|_| StorageError::invalid_input("max slots is out of range"))?,
                    i64::try_from(worker.running_slots).map_err(
                        |_| StorageError::invalid_input("running slots is out of range")
                    )?,
                    i64::try_from(worker.available_slots).map_err(|_| {
                        StorageError::invalid_input("available slots is out of range")
                    })?,
                ],
            )
            .map_err(sql_error)?;
        let scope_key = worker_scope_key(scope)?;
        transaction
            .execute(
                "INSERT INTO execution_worker_scopes (worker_id, scope_key, scope_json)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(worker_id) DO UPDATE SET
                    scope_key = excluded.scope_key,
                    scope_json = excluded.scope_json",
                params![worker.worker_id.0, scope_key, encode_json(scope)?],
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

    /// Persists the authenticated pool placement for one registered process.
    ///
    /// The caller supplies the identity returned by the transport
    /// authenticator. This method joins it to the current Worker registration
    /// before saving the secret-free placement receipt. Exact retries replay;
    /// any changed identity or pool is rejected.
    ///
    /// # Errors
    ///
    /// Rejects malformed, stale, foreign, or changed placement facts and
    /// `SQLite` failures.
    pub fn record_authenticated_worker_placement(
        &mut self,
        placement: &AuthenticatedWorkerPlacement,
    ) -> Result<AuthenticatedWorkerPlacement, StorageError> {
        validate_authenticated_placement(placement)?;
        let placement_digest = digest(placement)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let worker = load_worker_in_transaction(&transaction, &placement.worker_id)?
            .ok_or_else(|| StorageError::invalid_input("Worker placement has no registration"))?;
        if worker.worker_instance_id != placement.worker_instance_id
            || worker.management_scope != placement.management_scope
            || worker.authentication_identity != placement.authentication_identity
            || placement.placed_at.0 < worker.started_at.0
        {
            return Err(StorageError::invalid_input(
                "Worker placement differs from its authenticated registration",
            ));
        }
        if let Some(stored) = load_authenticated_placement_in_transaction(
            &transaction,
            &placement.worker_id,
            &placement.worker_instance_id,
        )? {
            if stored != *placement || digest(&stored)? != placement_digest {
                return Err(StorageError::invalid_input(
                    "Worker placement identity was reused with changed authority",
                ));
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(stored);
        }
        transaction
            .execute(
                "INSERT INTO execution_worker_authenticated_placements
                    (worker_id, worker_instance_id, worker_pool_id,
                     registration_request_id, placement_digest, record_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    placement.worker_id.0,
                    placement.worker_instance_id.0,
                    placement.worker_pool_id.0,
                    placement.registration_request_id.0,
                    placement_digest,
                    encode_json(placement)?,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(placement.clone())
    }

    /// Loads the authenticated pool placement for one exact Worker process.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, corrupt receipts, and `SQLite` failures.
    pub fn load_authenticated_worker_placement(
        &self,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
    ) -> Result<Option<AuthenticatedWorkerPlacement>, StorageError> {
        validate_id(&worker_id.0, "wrk_", "workerId")?;
        validate_id(&worker_instance_id.0, "wki_", "workerInstanceId")?;
        load_authenticated_placement_in_transaction(
            self.storage.connection()?,
            worker_id,
            worker_instance_id,
        )
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
        if current.max_slots != request.max_slots {
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerHeartbeatReceipt {
                status: LeaseWriteStatus::RejectedConflict,
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
            management_scope: current.management_scope,
            worker_instance_id: current.worker_instance_id.clone(),
            started_at: current.started_at,
            authentication_identity: current.authentication_identity,
            protocol_version: current.protocol_version,
            platform: current.platform,
            capabilities: current.capabilities,
            capability_digest: current.capability_digest,
            security_zone: current.security_zone,
            health: WorkerHealth::Healthy,
            last_heartbeat_at: Some(request.observed_at.clone()),
            heartbeat_sequence: sequence,
            max_slots: request.max_slots,
            running_slots: request.running_slots,
            available_slots: request.available_slots,
        };
        transaction
            .execute(
                "UPDATE execution_workers
                 SET health = ?1, last_heartbeat_at = ?2, heartbeat_sequence = ?3,
                     max_slots = ?4, running_slots = ?5, available_slots = ?6
                 WHERE worker_id = ?7 AND worker_instance_id = ?8",
                params![
                    WorkerHealth::Healthy.as_str(),
                    request.observed_at.0,
                    i64::try_from(sequence).map_err(|_| StorageError::invalid_input(
                        "heartbeat sequence is out of range"
                    ))?,
                    i64::try_from(request.max_slots)
                        .map_err(|_| StorageError::invalid_input("max slots is out of range"))?,
                    i64::try_from(request.running_slots).map_err(|_| {
                        StorageError::invalid_input("running slots is out of range")
                    })?,
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
        self.claim_execution_job_internal(request, false)
    }

    /// Claims one Job only through a persisted transport-authenticated pool
    /// placement and freezes that placement beside the accepted lease.
    ///
    /// # Errors
    ///
    /// Returns an input/adapter error for malformed or corrupt authority. A
    /// missing authenticated placement returns `RejectedWorkerInstance` before
    /// a lease row is written.
    pub fn claim_execution_job_with_authenticated_placement(
        &mut self,
        request: &ExecutionLeaseClaim,
    ) -> Result<ExecutionLeaseReceipt, StorageError> {
        self.claim_execution_job_internal(request, true)
    }

    #[allow(clippy::too_many_lines)]
    fn claim_execution_job_internal(
        &mut self,
        request: &ExecutionLeaseClaim,
        require_authenticated_placement: bool,
    ) -> Result<ExecutionLeaseReceipt, StorageError> {
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let response = claim_execution_job_in_transaction(
            &transaction,
            request,
            require_authenticated_placement,
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
        if execution_lease_is_terminal(&transaction, &current.lease_id)? {
            transaction.commit().map_err(sql_error)?;
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                Some(current),
                false,
            ));
        }
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

    /// Marks an exact fenced lease attempt terminal while retaining its
    /// immutable terminal fact for replay and forensic joins.
    ///
    /// Exact request replay returns `false`. Changed identities or outcomes
    /// fail closed and never change active capacity.
    ///
    /// # Errors
    ///
    /// Rejects malformed, foreign, stale-fence, or conflicting
    /// terminal requests and propagates `SQLite` failures.
    pub fn finish_execution_lease(
        &mut self,
        request: &ExecutionLeaseTerminalRequest,
    ) -> Result<bool, StorageError> {
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let inserted = finish_execution_lease_in_transaction(&transaction, request)?;
        transaction.commit().map_err(sql_error)?;
        Ok(inserted)
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
    pub fn record_dispatch_result(
        &mut self,
        request: &DispatchResultRequest,
    ) -> Result<DispatchResultReceipt, StorageError> {
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let response = record_dispatch_result_in_transaction(&transaction, request)?;
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

    /// Loads the accepted dispatch authority for one exact current Job lease.
    ///
    /// The record exists only after an accepted or exact duplicate dispatch
    /// result joined the registered Worker, current lease, and `WorkerSession` in
    /// the same Registry transaction.
    ///
    /// # Errors
    ///
    /// Rejects malformed/corrupt authority and propagates `SQLite` failures.
    pub fn load_dispatch_authority(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionDispatchAuthority>, StorageError> {
        validate_id(&job_id.0, "job_", "jobId")?;
        load_dispatch_authority_in_transaction(self.storage.connection()?, job_id)
    }

    /// Marks one exact Worker process instance offline after its authenticated
    /// transport disconnects.
    ///
    /// A stale connection for a replaced process returns `None` and cannot
    /// change the current instance. Repeating the exact disconnect returns the
    /// same timed-out Worker record without creating another identity source.
    ///
    /// # Errors
    ///
    /// Returns an input error for malformed identities or an adapter error when
    /// the single registry transaction fails.
    pub fn mark_worker_disconnected(
        &mut self,
        worker_id: &WorkerId,
        worker_instance_id: &WorkerInstanceId,
    ) -> Result<Option<WorkerRecord>, StorageError> {
        validate_id(&worker_id.0, "wrk_", "workerId")?;
        validate_id(&worker_instance_id.0, "wki_", "workerInstanceId")?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let Some(current) = load_worker_in_transaction(&transaction, worker_id)? else {
            transaction.commit().map_err(sql_error)?;
            return Ok(None);
        };
        if current.worker_instance_id != *worker_instance_id {
            transaction.commit().map_err(sql_error)?;
            return Ok(None);
        }
        if current.health != WorkerHealth::TimedOut {
            let changed = transaction
                .execute(
                    "UPDATE execution_workers SET health = ?1
                     WHERE worker_id = ?2 AND worker_instance_id = ?3",
                    params![
                        WorkerHealth::TimedOut.as_str(),
                        worker_id.0,
                        worker_instance_id.0,
                    ],
                )
                .map_err(sql_error)?;
            if changed != 1 {
                return Err(StorageError::adapter(
                    "Worker disconnect lost its registry authority",
                ));
            }
        }
        let worker = load_worker_in_transaction(&transaction, worker_id)?.ok_or_else(|| {
            StorageError::adapter("disconnected Worker disappeared from the registry")
        })?;
        transaction.commit().map_err(sql_error)?;
        Ok(Some(worker))
    }

    /// Loads one secret-free Worker management projection in an exact scope.
    ///
    /// # Errors
    ///
    /// Rejects malformed scope, Worker, or observation time and fails closed
    /// when durable Worker management state is corrupt.
    pub fn load_managed_worker(
        &self,
        scope: &WorkerRegistryScope,
        worker_id: &WorkerId,
        observed_at: &Instant,
    ) -> Result<Option<WorkerManagementSnapshot>, StorageError> {
        validate_worker_scope(scope)?;
        validate_id(&worker_id.0, "wrk_", "workerId")?;
        validate_instant(observed_at, "observedAt")?;
        let connection = self.storage.connection()?;
        let Some(worker) = load_worker_in_transaction(connection, worker_id)? else {
            return Ok(None);
        };
        if &worker.management_scope != scope {
            return Ok(None);
        }
        let record = load_worker_management_record(connection, &worker)?;
        management_snapshot(connection, &worker, &record, observed_at).map(Some)
    }

    /// Returns a stable exact-scope Worker page ordered by Worker identity.
    ///
    /// The first page seals an upper Worker identity; later registrations do
    /// not enter that walk. State filters are applied before the page limit.
    ///
    /// # Errors
    ///
    /// Rejects malformed scope, cursor, filters, page size, or observation
    /// time and fails closed on corrupt durable state.
    pub fn list_managed_workers(
        &self,
        scope: &WorkerRegistryScope,
        states: &[WorkerOperationalState],
        after: Option<&WorkerManagementPageCursor>,
        limit: usize,
        observed_at: &Instant,
    ) -> Result<WorkerManagementPage, StorageError> {
        validate_worker_page(scope, states, after, limit, observed_at)?;
        let connection = self.storage.connection()?;
        let scope_key = worker_scope_key(scope)?;
        let upper_bound = match after {
            Some(cursor) => cursor.upper_bound_worker_id.clone(),
            None => upper_bound_worker_id(connection, &scope_key)?
                .unwrap_or_else(|| WorkerId("wrk_00000000000000000000000000".to_owned())),
        };
        let after_worker = after.map_or("", |cursor| cursor.worker_id.0.as_str());
        let mut statement = connection
            .prepare(
                "SELECT workers.worker_id
                 FROM execution_workers AS workers
                 JOIN execution_worker_scopes AS scopes ON scopes.worker_id = workers.worker_id
                 WHERE scopes.scope_key = ?1 AND workers.worker_id > ?2
                   AND workers.worker_id <= ?3
                 ORDER BY workers.worker_id",
            )
            .map_err(sql_error)?;
        let worker_ids = statement
            .query_map(params![scope_key, after_worker, upper_bound.0], |row| {
                Ok(WorkerId(row.get(0)?))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        drop(statement);
        let mut workers = Vec::with_capacity(limit.saturating_add(1));
        for worker_id in worker_ids {
            let worker = load_worker_in_transaction(connection, &worker_id)?
                .ok_or_else(|| StorageError::adapter("scoped Worker row disappeared"))?;
            let record = load_worker_management_record(connection, &worker)?;
            let snapshot = management_snapshot(connection, &worker, &record, observed_at)?;
            if states.is_empty() || states.contains(&snapshot.operational_state) {
                workers.push(snapshot);
            }
            if workers.len() > limit {
                break;
            }
        }
        let has_more = workers.len() > limit;
        if has_more {
            workers.pop();
        }
        let next_cursor =
            has_more
                .then(|| workers.last())
                .flatten()
                .map(|worker| WorkerManagementPageCursor {
                    worker_id: worker.worker_id.clone(),
                    upper_bound_worker_id: upper_bound,
                });
        Ok(WorkerManagementPage {
            workers,
            next_cursor,
        })
    }

    /// Atomically changes placement state, saves an exact scoped receipt, and
    /// appends the caller-produced public Worker-health invalidation.
    ///
    /// The public event producer remains a Control Plane boundary because
    /// storage does not depend on generated WebSocket DTOs.
    ///
    /// # Errors
    ///
    /// Rejects malformed, foreign, repeated-state, stale, or conflicting
    /// commands and propagates durable commit failures.
    pub fn manage_worker(
        &mut self,
        command: &WorkerManagementCommand,
    ) -> Result<WorkerManagementReceipt, StorageError> {
        validate_management_command(command)?;
        if let Some(receipt) = self
            .storage
            .load_receipt(&command.receipt_identity, &command.command_digest)?
        {
            return decode_management_receipt(&receipt.events, true);
        }
        let connection = self.storage.connection()?;
        let worker = load_worker_in_transaction(connection, &command.worker_id)?
            .ok_or_else(|| StorageError::invalid_input("Worker was not found in its scope"))?;
        if worker.management_scope != command.scope {
            return Err(StorageError::invalid_input(
                "Worker was not found in its scope",
            ));
        }
        let current = load_worker_management_record(connection, &worker)?;
        if current.revision != command.expected_revision {
            return Err(StorageError::revision_conflict(
                command.expected_revision,
                current.revision,
            ));
        }
        if current.state == command.target_state {
            return Err(StorageError::invalid_input(
                "Worker is already in the requested management state",
            ));
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or_else(|| StorageError::invalid_input("Worker revision overflowed"))?;
        let record = WorkerManagementRecord {
            scope: command.scope.clone(),
            worker_id: command.worker_id.clone(),
            state: command.target_state,
            revision,
            updated_at: command.occurred_at.clone(),
        };
        let snapshot = management_snapshot(connection, &worker, &record, &command.occurred_at)?;
        let durable_receipt = WorkerManagementReceipt {
            previous_revision: current.revision,
            worker: snapshot,
            replayed: false,
        };
        let state = encode_json_bytes(&record)?;
        let receipt_payload = encode_json_bytes(&durable_receipt)?;
        let receipt_event_id = format!("worker-management-receipt:{}", command.command_digest.0);
        let commit = StateCommit::new(
            command.receipt_identity.clone(),
            command.command_digest.clone(),
            worker_management_stream_id(&command.scope, &command.worker_id)?,
            command.expected_revision,
            state,
            vec![
                NewOutboxEvent::internal(
                    receipt_event_id,
                    WORKER_MANAGEMENT_RECEIPT_TOPIC,
                    receipt_payload,
                ),
                command.public_event.clone(),
            ],
        );
        let receipt = self.storage.commit(&commit)?;
        decode_management_receipt(&receipt.events, receipt.idempotent_replay)
    }

    /// Replays one exact Worker management receipt without rebuilding its
    /// public event.
    ///
    /// # Errors
    ///
    /// Returns a request conflict when the scoped request identity already
    /// exists with a different command digest, and fails closed on corrupt
    /// durable receipt data.
    pub fn replay_worker_management(
        &self,
        receipt_identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<WorkerManagementReceipt>, StorageError> {
        self.storage
            .load_receipt(receipt_identity, command_digest)?
            .map(|receipt| decode_management_receipt(&receipt.events, true))
            .transpose()
    }

    /// Marks stale current instances timed out and returns one atomic capacity
    /// view. Lease and fencing rows are read or written by their existing
    /// methods only; this snapshot cannot mint or replace lease authority.
    ///
    /// # Errors
    ///
    /// Returns an input error for invalid times or an adapter error when the
    /// snapshot transaction fails.
    pub fn refresh_worker_capacity_snapshot(
        &mut self,
        observed_at: &Instant,
        stale_before: &Instant,
    ) -> Result<WorkerCapacitySnapshot, StorageError> {
        validate_instant(observed_at, "observedAt")?;
        validate_instant(stale_before, "staleBefore")?;
        if stale_before.0 > observed_at.0 {
            return Err(StorageError::invalid_input(
                "staleBefore must not follow observedAt",
            ));
        }
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        transaction
            .execute(
                "UPDATE execution_workers
                 SET health = ?1
                 WHERE COALESCE(last_heartbeat_at, started_at) < ?2",
                params![WorkerHealth::TimedOut.as_str(), stale_before.0],
            )
            .map_err(sql_error)?;

        let workers = {
            let mut statement = transaction
                .prepare(
                    "SELECT worker_id, worker_instance_id, protocol_version, platform,
                            capabilities, security_zone, health, max_slots, running_slots,
                            available_slots
                     FROM execution_workers ORDER BY worker_id",
                )
                .map_err(sql_error)?;
            statement
                .query_map([], |row| {
                    let platform = row.get::<_, String>(3)?;
                    let health = row.get::<_, String>(6)?;
                    let max_slots = row.get::<_, i64>(7)?;
                    let running_slots = row.get::<_, i64>(8)?;
                    let available_slots = row.get::<_, i64>(9)?;
                    Ok(WorkerCapacityEntry {
                        worker_id: WorkerId(row.get(0)?),
                        worker_instance_id: WorkerInstanceId(row.get(1)?),
                        protocol_version: row.get(2)?,
                        platform: WorkerPlatform::parse(&platform).ok_or_else(|| {
                            invalid_stored_text(platform.len(), "stored Worker platform is invalid")
                        })?,
                        capabilities: decode_json_row(&row.get::<_, String>(4)?)?,
                        security_zone: row.get(5)?,
                        health: WorkerHealth::parse(&health).ok_or_else(|| {
                            invalid_stored_text(health.len(), "stored Worker health is invalid")
                        })?,
                        max_slots: u64::try_from(max_slots)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(7, max_slots))?,
                        running_slots: u64::try_from(running_slots).map_err(|_| {
                            rusqlite::Error::IntegralValueOutOfRange(8, running_slots)
                        })?,
                        available_slots: u64::try_from(available_slots).map_err(|_| {
                            rusqlite::Error::IntegralValueOutOfRange(9, available_slots)
                        })?,
                    })
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?
        };

        let mut healthy_max_slots = 0_u64;
        let mut healthy_running_slots = 0_u64;
        let mut healthy_available_slots = 0_u64;
        for worker in workers
            .iter()
            .filter(|worker| worker.health == WorkerHealth::Healthy)
        {
            healthy_max_slots = checked_capacity_add(healthy_max_slots, worker.max_slots)?;
            healthy_running_slots =
                checked_capacity_add(healthy_running_slots, worker.running_slots)?;
            healthy_available_slots =
                checked_capacity_add(healthy_available_slots, worker.available_slots)?;
        }
        transaction.commit().map_err(sql_error)?;
        Ok(WorkerCapacitySnapshot {
            observed_at: observed_at.clone(),
            workers,
            healthy_max_slots,
            healthy_running_slots,
            healthy_available_slots,
        })
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

    /// Loads the authenticated pool placement frozen beside the current lease.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, corrupt placement receipts, and
    /// `SQLite` failures.
    pub fn load_lease_placement(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionLeasePlacement>, StorageError> {
        validate_id(&job_id.0, "job_", "jobId")?;
        let placement = load_lease_placement_in_transaction(self.storage.connection()?, job_id)?;
        if let Some(placement) = &placement {
            let lease = load_lease_in_transaction(self.storage.connection()?, job_id)?
                .ok_or_else(|| StorageError::adapter("lease placement lost its lease"))?;
            validate_lease_placement_binding(placement, &lease)?;
        }
        Ok(placement)
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

#[allow(clippy::too_many_lines)]
pub(crate) fn claim_execution_job_in_transaction(
    connection: &rusqlite::Connection,
    request: &ExecutionLeaseClaim,
    require_authenticated_placement: bool,
) -> Result<ExecutionLeaseReceipt, StorageError> {
    validate_claim(request)?;
    let request_digest = digest(request)?;
    if let Some(receipt) = lease_request_replay(
        connection,
        "claim",
        &request.job_id,
        &request.request_id,
        &request_digest,
    )? {
        if require_authenticated_placement {
            require_lease_placement_for_receipt(connection, &receipt)?;
        }
        return Ok(receipt);
    }
    if !worker_instance_is_current(connection, &request.worker_id, &request.worker_instance_id)? {
        return Ok(lease_receipt(
            LeaseWriteStatus::RejectedWorkerInstance,
            None,
            false,
        ));
    }
    let placement = load_authenticated_placement_in_transaction(
        connection,
        &request.worker_id,
        &request.worker_instance_id,
    )?;
    if require_authenticated_placement && placement.is_none() {
        return Ok(lease_receipt(
            LeaseWriteStatus::RejectedWorkerInstance,
            None,
            false,
        ));
    }
    if !worker_accepts_new_claim(connection, &request.worker_id)? {
        return Ok(lease_receipt(
            LeaseWriteStatus::RejectedConflict,
            None,
            false,
        ));
    }
    if connection
        .query_row(
            "SELECT 1 FROM execution_leases WHERE lease_id = ?1 AND job_id != ?2",
            params![request.lease_id.0, request.job_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some()
    {
        return Ok(lease_receipt(
            LeaseWriteStatus::RejectedConflict,
            None,
            false,
        ));
    }

    if let Some(current) = load_lease_in_transaction(connection, &request.job_id)? {
        let terminal = load_lease_terminal(connection, &current.lease_id)?;
        if terminal.as_ref().is_some_and(|terminal| {
            terminal.outcome != ExecutionLeaseTerminalOutcome::Failed
                || request.issued_at.0 < terminal.terminal_at.0
        }) {
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                Some(current),
                false,
            ));
        }
        let expired = terminal.is_some() || request.issued_at.0 >= current.expires_at.0;
        if less_decimal(&request.fencing_token.0, &current.fencing_token.0) {
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedStaleFencingToken,
                Some(current),
                false,
            ));
        }
        if !expired {
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                Some(current),
                false,
            ));
        }
        if request.attempt <= current.attempt {
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedExpiredLease,
                Some(current),
                false,
            ));
        }
        if request.lease_id == current.lease_id || request.fencing_token == current.fencing_token {
            return Ok(lease_receipt(
                LeaseWriteStatus::RejectedConflict,
                Some(current),
                false,
            ));
        }
        connection
            .execute(
                "DELETE FROM execution_dispatch_authorities
                 WHERE job_id = ?1 AND lease_id = ?2",
                params![current.job_id.0, current.lease_id.0],
            )
            .map_err(sql_error)?;
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
    connection
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
    if let Some(placement) = placement {
        persist_lease_placement(connection, &lease, &placement)?;
    } else {
        connection
            .execute(
                "DELETE FROM execution_lease_authenticated_placements WHERE job_id = ?1",
                [&lease.job_id.0],
            )
            .map_err(sql_error)?;
    }
    insert_lease_request_receipt(
        connection,
        "claim",
        &request.job_id,
        &request.request_id,
        &request_digest,
        &response,
    )?;
    Ok(response)
}

pub(crate) fn finish_execution_lease_in_transaction(
    connection: &rusqlite::Connection,
    request: &ExecutionLeaseTerminalRequest,
) -> Result<bool, StorageError> {
    validate_lease_terminal(request)?;
    let request_digest = digest(request)?;
    let existing = connection
        .query_row(
            "SELECT request_id, request_digest
             FROM execution_lease_terminals WHERE lease_id = ?1",
            [&request.lease_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    if let Some((request_id, stored_digest)) = existing {
        if request_id != request.request_id.0 || stored_digest != request_digest {
            return Err(StorageError::invalid_input(
                "execution lease terminal replay conflicts with its durable receipt",
            ));
        }
        return Ok(false);
    }

    let current = load_lease_in_transaction(connection, &request.job_id)?
        .ok_or_else(|| StorageError::invalid_input("execution lease does not exist"))?;
    if current.lease_id != request.lease_id
        || current.worker_id != request.worker_id
        || current.worker_instance_id != request.worker_instance_id
        || current.attempt != request.attempt
        || current.fencing_token != request.fencing_token
    {
        return Err(StorageError::invalid_input(
            "execution lease terminal does not match current fenced authority",
        ));
    }
    if request.terminal_at.0 < current.issued_at.0 {
        return Err(StorageError::invalid_input(
            "execution lease terminal predates the accepted lease",
        ));
    }

    connection
        .execute(
            "INSERT INTO execution_lease_terminals (
                 lease_id, job_id, worker_id, worker_instance_id, attempt,
                 fencing_token, outcome, terminal_at, request_id, request_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                request.lease_id.0,
                request.job_id.0,
                request.worker_id.0,
                request.worker_instance_id.0,
                i64::try_from(request.attempt)
                    .map_err(|_| StorageError::invalid_input("attempt is out of range"))?,
                request.fencing_token.0,
                request.outcome.as_str(),
                request.terminal_at.0,
                request.request_id.0,
                request_digest,
            ],
        )
        .map_err(sql_error)?;
    Ok(true)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn record_dispatch_result_in_transaction(
    connection: &rusqlite::Connection,
    request: &DispatchResultRequest,
) -> Result<DispatchResultReceipt, StorageError> {
    let request_digest = dispatch_result_digest(request)?;
    if let Some(receipt) = dispatch_result_replay(
        connection,
        &request.job_id,
        &request.request_id,
        &request_digest,
    )? {
        return Ok(receipt);
    }
    validate_dispatch_result(request)?;

    let Some(worker) = load_worker_in_transaction(connection, &request.worker_id)? else {
        return Ok(dispatch_result_rejection(
            DispatchResultStatus::RejectedWorkerInstance,
            DispatchResultErrorCode::WorkerNotRegistered,
        ));
    };
    if worker.worker_instance_id != request.worker_instance_id {
        return Ok(dispatch_result_rejection(
            DispatchResultStatus::RejectedWorkerInstance,
            DispatchResultErrorCode::WorkerInstanceChanged,
        ));
    }

    let Some(current) = load_lease_in_transaction(connection, &request.job_id)? else {
        return Ok(dispatch_result_rejection(
            DispatchResultStatus::Conflict,
            DispatchResultErrorCode::JobDispatchConflict,
        ));
    };
    if execution_lease_is_terminal(connection, &current.lease_id)? {
        return Ok(dispatch_result_rejection(
            DispatchResultStatus::Conflict,
            DispatchResultErrorCode::JobDispatchConflict,
        ));
    }
    if less_decimal(&request.fencing_token.0, &current.fencing_token.0) {
        return Ok(dispatch_result_rejection(
            DispatchResultStatus::RejectedStaleFencingToken,
            DispatchResultErrorCode::StaleFencingToken,
        ));
    }
    if request.checked_at.0 >= current.expires_at.0 || request.sent_at.0 >= current.expires_at.0 {
        return Ok(dispatch_result_rejection(
            DispatchResultStatus::RejectedExpiredLease,
            DispatchResultErrorCode::LeaseExpired,
        ));
    }
    if request.sent_at.0 < current.issued_at.0 {
        return Ok(dispatch_result_rejection(
            DispatchResultStatus::Conflict,
            DispatchResultErrorCode::JobDispatchConflict,
        ));
    }
    if request.payload_digest != current.payload_digest {
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
    if matches!(
        request.status,
        DispatchResultStatus::Accepted | DispatchResultStatus::Duplicate
    ) {
        let worker_session_id = request.worker_session_id.as_ref().ok_or_else(|| {
            StorageError::invalid_input("accepted dispatch result has no WorkerSession authority")
        })?;
        insert_dispatch_authority(
            connection,
            &current,
            worker_session_id,
            &request.request_id,
            &request.checked_at,
        )?;
    }
    insert_dispatch_result_receipt(
        connection,
        &request.job_id,
        &request.request_id,
        &request_digest,
        response,
    )?;
    Ok(response)
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
    validate_authentication_identity(&request.authentication_identity)?;
    validate_capabilities(&request.capabilities)?;
    validate_digest(&request.capability_digest)?;
    validate_bounded_ascii(&request.protocol_version, "protocolVersion")?;
    validate_bounded_ascii(&request.security_zone, "securityZone")?;
    if request.max_slots == 0 || request.max_slots > MAX_WORKER_SLOTS {
        return Err(StorageError::invalid_input("worker max slots are invalid"));
    }
    Ok(())
}

fn validate_authenticated_placement(
    placement: &AuthenticatedWorkerPlacement,
) -> Result<(), StorageError> {
    validate_id(&placement.worker_id.0, "wrk_", "workerId")?;
    validate_id(&placement.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_id(&placement.worker_pool_id.0, "wpl_", "workerPoolId")?;
    validate_id(
        &placement.registration_request_id.0,
        "req_",
        "registrationRequestId",
    )?;
    validate_worker_scope(&placement.management_scope)?;
    validate_authentication_identity(&placement.authentication_identity)?;
    if !matches!(
        placement.authentication_identity,
        WorkerAuthenticationIdentity::TransportPrincipal { .. }
    ) {
        return Err(StorageError::invalid_input(
            "authenticated Worker placement requires a transport principal",
        ));
    }
    validate_instant(&placement.placed_at, "placedAt")
}

fn validate_worker_scope(scope: &WorkerRegistryScope) -> Result<(), StorageError> {
    match scope {
        WorkerRegistryScope::Organization { organization_id } => {
            validate_id(&organization_id.0, "org_", "scope.organizationId")
        }
        WorkerRegistryScope::Workspace {
            organization_id,
            workspace_id,
        } => {
            validate_id(&organization_id.0, "org_", "scope.organizationId")?;
            validate_id(&workspace_id.0, "wsp_", "scope.workspaceId")
        }
        WorkerRegistryScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => {
            validate_id(&organization_id.0, "org_", "scope.organizationId")?;
            validate_id(&workspace_id.0, "wsp_", "scope.workspaceId")?;
            validate_id(&project_id.0, "prj_", "scope.projectId")
        }
        WorkerRegistryScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => {
            validate_id(&organization_id.0, "org_", "scope.organizationId")?;
            validate_id(&workspace_id.0, "wsp_", "scope.workspaceId")?;
            validate_id(&project_id.0, "prj_", "scope.projectId")?;
            validate_id(&repository_id.0, "rep_", "scope.repositoryId")
        }
    }
}

fn worker_public_scope(scope: &WorkerRegistryScope) -> PublicEventScope {
    match scope {
        WorkerRegistryScope::Organization { organization_id } => PublicEventScope::Organization {
            organization_id: organization_id.clone(),
        },
        WorkerRegistryScope::Workspace {
            organization_id,
            workspace_id,
        } => PublicEventScope::Workspace {
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
        },
        WorkerRegistryScope::Project {
            organization_id,
            workspace_id,
            project_id,
        } => PublicEventScope::Project {
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
        },
        WorkerRegistryScope::Repository {
            organization_id,
            workspace_id,
            project_id,
            repository_id,
        } => PublicEventScope::Repository {
            organization_id: organization_id.clone(),
            workspace_id: workspace_id.clone(),
            project_id: project_id.clone(),
            repository_id: repository_id.clone(),
        },
    }
}

fn validate_management_command(command: &WorkerManagementCommand) -> Result<(), StorageError> {
    validate_worker_scope(&command.scope)?;
    validate_id(&command.worker_id.0, "wrk_", "workerId")?;
    validate_digest(&command.command_digest)?;
    validate_instant(&command.occurred_at, "occurredAt")?;
    if command.expected_revision > i64::MAX as u64 {
        return Err(StorageError::invalid_input(
            "Worker expectedRevision is out of range",
        ));
    }
    let expected_scope = worker_public_scope(&command.scope);
    if receipt_scope_key(&expected_scope)? != *command.receipt_identity.scope_key() {
        return Err(StorageError::invalid_input(
            "Worker command receipt scope differs from Worker scope",
        ));
    }
    let context = command.public_event.public_context().ok_or_else(|| {
        StorageError::invalid_input("Worker management requires one public outbox event")
    })?;
    if context.scope() != &expected_scope || context.occurred_at() != &command.occurred_at {
        return Err(StorageError::invalid_input(
            "Worker public event context differs from its command",
        ));
    }
    if command.public_event.topic != "worker-health.changed.v1"
        || command.public_event.projection_stream() != Some(&ProjectionEventStream::Scope)
    {
        return Err(StorageError::invalid_input(
            "Worker management public event is invalid",
        ));
    }
    Ok(())
}

fn validate_worker_page(
    scope: &WorkerRegistryScope,
    states: &[WorkerOperationalState],
    after: Option<&WorkerManagementPageCursor>,
    limit: usize,
    observed_at: &Instant,
) -> Result<(), StorageError> {
    validate_worker_scope(scope)?;
    validate_instant(observed_at, "observedAt")?;
    if limit == 0 || limit > MAX_WORKER_PAGE_SIZE {
        return Err(StorageError::invalid_input(
            "Worker page size is outside the supported range",
        ));
    }
    let mut unique = HashSet::new();
    if states.iter().any(|state| !unique.insert(*state)) {
        return Err(StorageError::invalid_input(
            "Worker state filter contains duplicates",
        ));
    }
    if let Some(cursor) = after {
        validate_id(&cursor.worker_id.0, "wrk_", "cursor.workerId")?;
        validate_id(
            &cursor.upper_bound_worker_id.0,
            "wrk_",
            "cursor.upperBoundWorkerId",
        )?;
        if cursor.worker_id.0 > cursor.upper_bound_worker_id.0 {
            return Err(StorageError::invalid_input("Worker cursor is invalid"));
        }
    }
    Ok(())
}

fn worker_scope_key(scope: &WorkerRegistryScope) -> Result<String, StorageError> {
    validate_worker_scope(scope)?;
    encode_json(scope).map(|encoded| format!("worker-scope:{encoded}"))
}

fn worker_management_stream_id(
    scope: &WorkerRegistryScope,
    worker_id: &WorkerId,
) -> Result<String, StorageError> {
    let scope_key = worker_scope_key(scope)?;
    let digest = Sha256::digest(scope_key.as_bytes());
    Ok(format!(
        "{WORKER_MANAGEMENT_STREAM_PREFIX}{digest:x}:{}",
        worker_id.0
    ))
}

fn registration_digest(
    request: &WorkerRegistrationRequest,
    scope: &WorkerRegistryScope,
) -> Result<String, StorageError> {
    #[derive(Serialize)]
    struct RegistrationDigest<'a> {
        request: &'a WorkerRegistrationRequest,
        scope: &'a WorkerRegistryScope,
    }
    digest(&RegistrationDigest { request, scope })
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
    if request.max_slots == 0
        || request.max_slots > MAX_WORKER_SLOTS
        || request.running_slots > MAX_WORKER_SLOTS
        || request.available_slots > MAX_WORKER_SLOTS
    {
        return Err(StorageError::invalid_input(
            "worker slots exceed the maximum",
        ));
    }
    if request.running_slots.checked_add(request.available_slots) != Some(request.max_slots) {
        return Err(StorageError::invalid_input(
            "running and available slots must equal max slots",
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

fn validate_lease_terminal(request: &ExecutionLeaseTerminalRequest) -> Result<(), StorageError> {
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.lease_id.0, "lse_", "leaseId")?;
    validate_id(&request.worker_id.0, "wrk_", "workerId")?;
    validate_id(&request.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_instant(&request.terminal_at, "terminalAt")?;
    if request.attempt == 0 || request.attempt > 1_000 {
        return Err(StorageError::invalid_input("attempt is invalid"));
    }
    validate_fencing_token(&request.fencing_token)
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

fn validate_authentication_identity(
    identity: &WorkerAuthenticationIdentity,
) -> Result<(), StorageError> {
    match identity {
        WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal,
        } => validate_bounded_ascii(
            control_plane_principal,
            "authentication.controlPlanePrincipal",
        ),
        WorkerAuthenticationIdentity::TransportPrincipal {
            issuer,
            subject,
            credential_fingerprint,
        } => {
            validate_bounded_ascii(issuer, "authentication.issuer")?;
            validate_bounded_ascii(subject, "authentication.subject")?;
            validate_digest(credential_fingerprint)
        }
    }
}

fn validate_bounded_ascii(value: &str, field: &str) -> Result<(), StorageError> {
    if value.is_empty()
        || value.len() > 200
        || value.trim() != value
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(StorageError::invalid_input(format!("{field} is invalid")));
    }
    Ok(())
}

fn registration_profile_rejection(
    request: &WorkerRegistrationRequest,
) -> Option<WorkerRegistrationErrorCode> {
    (request.protocol_version != EXECUTION_PROTOCOL_VERSION)
        .then_some(WorkerRegistrationErrorCode::ProtocolVersionUnsupported)
}

fn registration_profile_conflict(
    worker: &WorkerRecord,
    request: &WorkerRegistrationRequest,
    scope: &WorkerRegistryScope,
) -> Option<WorkerRegistrationErrorCode> {
    if &worker.management_scope != scope {
        return Some(WorkerRegistrationErrorCode::ScopeMismatch);
    }
    if worker.authentication_identity != request.authentication_identity {
        return Some(WorkerRegistrationErrorCode::AuthenticationMismatch);
    }
    if worker.protocol_version != request.protocol_version {
        return Some(WorkerRegistrationErrorCode::ProtocolVersionUnsupported);
    }
    if worker.security_zone != request.security_zone {
        return Some(WorkerRegistrationErrorCode::SecurityZoneMismatch);
    }
    if worker.platform != request.platform
        || worker.capabilities != request.capabilities
        || worker.capability_digest != request.capability_digest
        || worker.max_slots != request.max_slots
    {
        return Some(WorkerRegistrationErrorCode::CapabilityMismatch);
    }
    None
}

fn registration_conflict_code(
    worker: &WorkerRecord,
    request: &WorkerRegistrationRequest,
    scope: &WorkerRegistryScope,
) -> WorkerRegistrationErrorCode {
    registration_profile_conflict(worker, request, scope)
        .unwrap_or(WorkerRegistrationErrorCode::MessageConflict)
}

fn registration_rejection(
    worker: WorkerRecord,
    error: WorkerRegistrationErrorCode,
) -> WorkerRegistrationReceipt {
    WorkerRegistrationReceipt {
        status: WorkerRegistrationStatus::RejectedConflict,
        error: Some(error),
        lease_recovery: LeaseRecovery::NoActiveLeases,
        worker,
    }
}

fn checked_capacity_add(left: u64, right: u64) -> Result<u64, StorageError> {
    left.checked_add(right)
        .ok_or_else(|| StorageError::adapter("Worker capacity snapshot overflowed"))
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

fn empty_worker_record(
    request: &WorkerRegistrationRequest,
    scope: &WorkerRegistryScope,
) -> WorkerRecord {
    WorkerRecord {
        worker_id: request.worker_id.clone(),
        management_scope: scope.clone(),
        worker_instance_id: request.worker_instance_id.clone(),
        started_at: request.started_at.clone(),
        authentication_identity: request.authentication_identity.clone(),
        protocol_version: request.protocol_version.clone(),
        platform: request.platform,
        capabilities: request.capabilities.clone(),
        capability_digest: request.capability_digest.clone(),
        security_zone: request.security_zone.clone(),
        health: WorkerHealth::Registered,
        last_heartbeat_at: None,
        heartbeat_sequence: 0,
        max_slots: request.max_slots,
        running_slots: 0,
        available_slots: request.max_slots,
    }
}

fn validate_worker_record(worker: &WorkerRecord) -> Result<(), StorageError> {
    validate_id(&worker.worker_id.0, "wrk_", "workerId")?;
    validate_id(&worker.worker_instance_id.0, "wki_", "workerInstanceId")?;
    validate_instant(&worker.started_at, "startedAt")?;
    if let Some(last_heartbeat_at) = &worker.last_heartbeat_at {
        validate_instant(last_heartbeat_at, "lastHeartbeatAt")?;
    }
    validate_authentication_identity(&worker.authentication_identity)?;
    validate_bounded_ascii(&worker.protocol_version, "protocolVersion")?;
    if worker.protocol_version != EXECUTION_PROTOCOL_VERSION {
        return Err(StorageError::invalid_input(
            "Worker protocol version is unsupported",
        ));
    }
    validate_capabilities(&worker.capabilities)?;
    validate_digest(&worker.capability_digest)?;
    validate_bounded_ascii(&worker.security_zone, "securityZone")?;
    if worker.heartbeat_sequence > MAX_EXECUTION_SEQUENCE {
        return Err(StorageError::invalid_input(
            "heartbeat sequence is out of range",
        ));
    }
    if (worker.heartbeat_sequence == 0) != worker.last_heartbeat_at.is_none()
        || (worker.health == WorkerHealth::Registered && worker.heartbeat_sequence != 0)
        || (worker.health == WorkerHealth::Healthy && worker.last_heartbeat_at.is_none())
    {
        return Err(StorageError::invalid_input(
            "Worker heartbeat health state is invalid",
        ));
    }
    if worker.max_slots == 0
        || worker.max_slots > MAX_WORKER_SLOTS
        || worker.running_slots > MAX_WORKER_SLOTS
        || worker.available_slots > MAX_WORKER_SLOTS
        || worker.running_slots.checked_add(worker.available_slots) != Some(worker.max_slots)
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
            "SELECT workers.worker_id, worker_instance_id, started_at,
                    authentication_identity, protocol_version, platform, capabilities,
                    capability_digest, security_zone, health, last_heartbeat_at,
                    heartbeat_sequence, max_slots, running_slots, available_slots,
                    scopes.scope_json
             FROM execution_workers AS workers
             JOIN execution_worker_scopes AS scopes ON scopes.worker_id = workers.worker_id
             WHERE workers.worker_id = ?1",
            params![worker_id.0],
            |row| {
                let platform = row.get::<_, String>(5)?;
                let health = row.get::<_, String>(9)?;
                let heartbeat_sequence = row.get::<_, i64>(11)?;
                let max_slots = row.get::<_, i64>(12)?;
                let running_slots = row.get::<_, i64>(13)?;
                let available_slots = row.get::<_, i64>(14)?;
                Ok(WorkerRecord {
                    worker_id: WorkerId(row.get(0)?),
                    management_scope: decode_json_row(&row.get::<_, String>(15)?)?,
                    worker_instance_id: WorkerInstanceId(row.get(1)?),
                    started_at: Instant(row.get(2)?),
                    authentication_identity: decode_json_row(&row.get::<_, String>(3)?)?,
                    protocol_version: row.get(4)?,
                    platform: WorkerPlatform::parse(&platform).ok_or_else(|| {
                        invalid_stored_text(platform.len(), "stored Worker platform is invalid")
                    })?,
                    capabilities: decode_json_row(&row.get::<_, String>(6)?)?,
                    capability_digest: Sha256Digest(row.get(7)?),
                    security_zone: row.get(8)?,
                    health: WorkerHealth::parse(&health).ok_or_else(|| {
                        invalid_stored_text(health.len(), "stored Worker health is invalid")
                    })?,
                    last_heartbeat_at: row.get::<_, Option<String>>(10)?.map(Instant),
                    heartbeat_sequence: u64::try_from(heartbeat_sequence).map_err(|_| {
                        rusqlite::Error::IntegralValueOutOfRange(0, heartbeat_sequence)
                    })?,
                    max_slots: u64::try_from(max_slots)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, max_slots))?,
                    running_slots: u64::try_from(running_slots)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, running_slots))?,
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

fn load_worker_management_record(
    connection: &rusqlite::Connection,
    worker: &WorkerRecord,
) -> Result<WorkerManagementRecord, StorageError> {
    let stream_id = worker_management_stream_id(&worker.management_scope, &worker.worker_id)?;
    let stored = connection
        .query_row(
            "SELECT revision, payload FROM product_state WHERE stream_id = ?1",
            [&stream_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((revision, payload)) = stored else {
        return Ok(WorkerManagementRecord {
            scope: worker.management_scope.clone(),
            worker_id: worker.worker_id.clone(),
            state: WorkerManagementState::Enabled,
            revision: 0,
            updated_at: worker.started_at.clone(),
        });
    };
    let revision = u64::try_from(revision)
        .map_err(|_| StorageError::adapter("stored Worker revision is negative"))?;
    let record: WorkerManagementRecord = serde_json::from_slice(&payload)
        .map_err(|_| StorageError::adapter("stored Worker management state is corrupt"))?;
    if record.scope != worker.management_scope
        || record.worker_id != worker.worker_id
        || record.revision != revision
    {
        return Err(StorageError::adapter(
            "stored Worker management authority is inconsistent",
        ));
    }
    validate_instant(&record.updated_at, "management.updatedAt")
        .map_err(|_| StorageError::adapter("stored Worker management time is corrupt"))?;
    Ok(record)
}

fn management_snapshot(
    connection: &rusqlite::Connection,
    worker: &WorkerRecord,
    record: &WorkerManagementRecord,
    observed_at: &Instant,
) -> Result<WorkerManagementSnapshot, StorageError> {
    let operational_state = if record.state == WorkerManagementState::Draining {
        WorkerOperationalState::Draining
    } else if worker.health == WorkerHealth::TimedOut {
        WorkerOperationalState::Offline
    } else {
        WorkerOperationalState::Enabled
    };
    let active_lease_count = connection
        .query_row(
            "SELECT COUNT(*) FROM execution_leases AS leases
             WHERE leases.worker_id = ?1 AND leases.expires_at > ?2
               AND NOT EXISTS (
                   SELECT 1 FROM execution_lease_terminals AS terminals
                   WHERE terminals.lease_id = leases.lease_id
               )",
            params![worker.worker_id.0, observed_at.0],
            |row| row.get::<_, i64>(0),
        )
        .map_err(sql_error)?;
    let active_lease_count = u64::try_from(active_lease_count)
        .map_err(|_| StorageError::adapter("Worker active lease count is negative"))?;
    let available_capacity = if operational_state == WorkerOperationalState::Enabled {
        worker.available_slots
    } else {
        0
    };
    Ok(WorkerManagementSnapshot {
        worker_id: worker.worker_id.clone(),
        scope: record.scope.clone(),
        management_state: record.state,
        operational_state,
        health: worker.health,
        revision: record.revision,
        capacity: worker.max_slots,
        available_capacity,
        active_lease_count,
        last_heartbeat_at: worker.last_heartbeat_at.clone(),
        observed_at: observed_at.clone(),
    })
}

fn upper_bound_worker_id(
    connection: &rusqlite::Connection,
    scope_key: &str,
) -> Result<Option<WorkerId>, StorageError> {
    connection
        .query_row(
            "SELECT MAX(worker_id) FROM execution_worker_scopes WHERE scope_key = ?1",
            [scope_key],
            |row| row.get::<_, Option<String>>(0),
        )
        .map(|value| value.map(WorkerId))
        .map_err(sql_error)
}

fn encode_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
    serde_json::to_vec(value)
        .map_err(|_| StorageError::adapter("failed to encode Worker management value"))
}

fn decode_management_receipt(
    events: &[crate::OutboxEvent],
    replayed: bool,
) -> Result<WorkerManagementReceipt, StorageError> {
    let mut matching = events
        .iter()
        .filter(|event| event.topic == WORKER_MANAGEMENT_RECEIPT_TOPIC);
    let event = matching
        .next()
        .ok_or_else(|| StorageError::adapter("Worker management receipt event is missing"))?;
    if matching.next().is_some() {
        return Err(StorageError::adapter(
            "Worker management receipt event is duplicated",
        ));
    }
    let mut receipt: WorkerManagementReceipt = serde_json::from_slice(&event.payload)
        .map_err(|_| StorageError::adapter("Worker management receipt is corrupt"))?;
    receipt.replayed = replayed;
    Ok(receipt)
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

fn invalid_stored_text(length: usize, message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        length,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message,
        )),
    )
}

pub(crate) fn load_lease_in_transaction(
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

fn load_authenticated_placement_in_transaction(
    connection: &rusqlite::Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<Option<AuthenticatedWorkerPlacement>, StorageError> {
    let stored = connection
        .query_row(
            "SELECT worker_pool_id, registration_request_id, placement_digest, record_json
             FROM execution_worker_authenticated_placements
             WHERE worker_id = ?1 AND worker_instance_id = ?2",
            params![worker_id.0, worker_instance_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((stored_pool_id, stored_request_id, stored_digest, record_json)) = stored else {
        return Ok(None);
    };
    let placement = decode_json::<AuthenticatedWorkerPlacement>(&record_json)?;
    validate_authenticated_placement(&placement)
        .map_err(|_| StorageError::adapter("stored Worker placement is invalid"))?;
    if placement.worker_id != *worker_id
        || placement.worker_instance_id != *worker_instance_id
        || placement.worker_pool_id.0 != stored_pool_id
        || placement.registration_request_id.0 != stored_request_id
        || digest(&placement)? != stored_digest
        || encode_json(&placement)? != record_json
    {
        return Err(StorageError::adapter(
            "stored Worker placement differs from its authority columns",
        ));
    }
    Ok(Some(placement))
}

fn persist_lease_placement(
    transaction: &rusqlite::Connection,
    lease: &ExecutionLeaseRecord,
    worker_placement: &AuthenticatedWorkerPlacement,
) -> Result<(), StorageError> {
    let record = ExecutionLeasePlacement {
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_pool_id: worker_placement.worker_pool_id.clone(),
        worker_placement_digest: Sha256Digest(digest(worker_placement)?),
        claimed_at: lease.issued_at.clone(),
    };
    validate_lease_placement_binding(&record, lease)?;
    transaction
        .execute(
            "INSERT INTO execution_lease_authenticated_placements
                (job_id, lease_id, placement_digest, record_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(job_id) DO UPDATE SET
                lease_id = excluded.lease_id,
                placement_digest = excluded.placement_digest,
                record_json = excluded.record_json",
            params![
                record.job_id.0,
                record.lease_id.0,
                digest(&record)?,
                encode_json(&record)?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn load_lease_placement_in_transaction(
    connection: &rusqlite::Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionLeasePlacement>, StorageError> {
    let stored = connection
        .query_row(
            "SELECT lease_id, placement_digest, record_json
             FROM execution_lease_authenticated_placements WHERE job_id = ?1",
            [&job_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((stored_lease_id, stored_digest, record_json)) = stored else {
        return Ok(None);
    };
    let placement = decode_json::<ExecutionLeasePlacement>(&record_json)?;
    if placement.job_id != *job_id
        || placement.lease_id.0 != stored_lease_id
        || digest(&placement)? != stored_digest
        || encode_json(&placement)? != record_json
    {
        return Err(StorageError::adapter(
            "stored lease placement differs from its authority columns",
        ));
    }
    Ok(Some(placement))
}

fn validate_lease_placement_binding(
    placement: &ExecutionLeasePlacement,
    lease: &ExecutionLeaseRecord,
) -> Result<(), StorageError> {
    validate_id(&placement.worker_pool_id.0, "wpl_", "workerPoolId")?;
    validate_digest(&placement.worker_placement_digest)?;
    validate_instant(&placement.claimed_at, "claimedAt")?;
    if placement.job_id != lease.job_id
        || placement.lease_id != lease.lease_id
        || placement.worker_id != lease.worker_id
        || placement.worker_instance_id != lease.worker_instance_id
        || placement.claimed_at != lease.issued_at
    {
        return Err(StorageError::adapter(
            "lease placement differs from its current Registry lease",
        ));
    }
    Ok(())
}

fn require_lease_placement_for_receipt(
    connection: &rusqlite::Connection,
    receipt: &ExecutionLeaseReceipt,
) -> Result<(), StorageError> {
    let lease = receipt.lease.as_ref().ok_or_else(|| {
        StorageError::adapter("authenticated lease replay lost its accepted lease")
    })?;
    let placement =
        load_lease_placement_in_transaction(connection, &lease.job_id)?.ok_or_else(|| {
            StorageError::adapter("authenticated lease replay lost its pool placement")
        })?;
    validate_lease_placement_binding(&placement, lease)
}

fn has_old_worker_leases(
    connection: &rusqlite::Connection,
    worker_id: &WorkerId,
    worker_instance_id: &WorkerInstanceId,
) -> Result<bool, StorageError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM execution_leases AS leases
             WHERE leases.worker_id = ?1 AND leases.worker_instance_id != ?2
               AND NOT EXISTS (
                   SELECT 1 FROM execution_lease_terminals AS terminals
                   WHERE terminals.lease_id = leases.lease_id
               )
             LIMIT 1",
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
        if execution_lease_is_terminal(connection, &summary.lease_id)? {
            return Ok(Some(LeaseWriteStatus::RejectedConflict));
        }
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
        if summary.lease_id != current.lease_id
            || summary.attempt != current.attempt
            || summary.fencing_token != current.fencing_token
        {
            return Ok(Some(LeaseWriteStatus::RejectedConflict));
        }
        if request.observed_at.0 >= current.expires_at.0 {
            return Ok(Some(LeaseWriteStatus::RejectedExpiredLease));
        }
    }
    Ok(None)
}

fn execution_lease_is_terminal(
    connection: &rusqlite::Connection,
    lease_id: &LeaseId,
) -> Result<bool, StorageError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM execution_lease_terminals WHERE lease_id = ?1",
            [&lease_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some())
}

fn load_lease_terminal(
    connection: &rusqlite::Connection,
    lease_id: &LeaseId,
) -> Result<Option<ExecutionLeaseTerminalRecord>, StorageError> {
    connection
        .query_row(
            "SELECT outcome, terminal_at FROM execution_lease_terminals WHERE lease_id = ?1",
            [&lease_id.0],
            |row| {
                let outcome = row.get::<_, String>(0)?;
                Ok(ExecutionLeaseTerminalRecord {
                    outcome: match outcome.as_str() {
                        "completed" => ExecutionLeaseTerminalOutcome::Completed,
                        "cancelled" => ExecutionLeaseTerminalOutcome::Cancelled,
                        "failed" => ExecutionLeaseTerminalOutcome::Failed,
                        _ => {
                            return Err(invalid_stored_text(
                                outcome.len(),
                                "stored execution lease terminal outcome is invalid",
                            ));
                        }
                    },
                    terminal_at: Instant(row.get(1)?),
                })
            },
        )
        .optional()
        .map_err(sql_error)
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

fn worker_accepts_new_claim(
    connection: &rusqlite::Connection,
    worker_id: &WorkerId,
) -> Result<bool, StorageError> {
    let worker = load_worker_in_transaction(connection, worker_id)?
        .ok_or_else(|| StorageError::adapter("current Worker scope is missing"))?;
    let management = load_worker_management_record(connection, &worker)?;
    Ok(management.state == WorkerManagementState::Enabled)
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
    connection: &rusqlite::Connection,
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

fn insert_dispatch_authority(
    connection: &rusqlite::Connection,
    lease: &ExecutionLeaseRecord,
    worker_session_id: &WorkerSessionId,
    dispatch_request_id: &RequestId,
    accepted_at: &Instant,
) -> Result<(), StorageError> {
    let existing = load_dispatch_authority_in_transaction(connection, &lease.job_id)?;
    if let Some(existing) = existing {
        if existing.lease != *lease
            || existing.worker_session_id != *worker_session_id
            || existing.dispatch_request_id != *dispatch_request_id
        {
            return Err(StorageError::invalid_input(
                "execution dispatch authority conflicts with the accepted result",
            ));
        }
        return Ok(());
    }
    connection
        .execute(
            "INSERT INTO execution_dispatch_authorities
                (job_id, lease_id, payload_digest, worker_id, worker_instance_id,
                 worker_session_id, attempt, fencing_token, issued_at, expires_at,
                 dispatch_request_id, accepted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                lease.job_id.0,
                lease.lease_id.0,
                lease.payload_digest.0,
                lease.worker_id.0,
                lease.worker_instance_id.0,
                worker_session_id.0,
                i64::try_from(lease.attempt)
                    .map_err(|_| StorageError::invalid_input("attempt is out of range"))?,
                lease.fencing_token.0,
                lease.issued_at.0,
                lease.expires_at.0,
                dispatch_request_id.0,
                accepted_at.0,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

pub(crate) fn load_dispatch_authority_in_transaction(
    connection: &rusqlite::Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionDispatchAuthority>, StorageError> {
    let stored = connection
        .query_row(
            "SELECT lease_id, payload_digest, worker_id, worker_instance_id,
                    worker_session_id, attempt, fencing_token, issued_at, expires_at,
                    dispatch_request_id, accepted_at
             FROM execution_dispatch_authorities WHERE job_id = ?1",
            [&job_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((
        lease_id,
        payload_digest,
        worker_id,
        worker_instance_id,
        worker_session_id,
        attempt,
        fencing_token,
        issued_at,
        expires_at,
        dispatch_request_id,
        accepted_at,
    )) = stored
    else {
        return Ok(None);
    };
    let attempt = u64::try_from(attempt)
        .ok()
        .filter(|attempt| (1..=MAX_LEASE_ATTEMPT).contains(attempt))
        .ok_or_else(|| StorageError::adapter("stored dispatch attempt is invalid"))?;
    let authority = ExecutionDispatchAuthority {
        lease: ExecutionLeaseRecord {
            job_id: job_id.clone(),
            lease_id: LeaseId(lease_id),
            payload_digest: Sha256Digest(payload_digest),
            worker_id: WorkerId(worker_id),
            worker_instance_id: WorkerInstanceId(worker_instance_id),
            attempt,
            fencing_token: FencingToken(fencing_token),
            issued_at: Instant(issued_at),
            expires_at: Instant(expires_at),
        },
        worker_session_id: WorkerSessionId(worker_session_id),
        dispatch_request_id: RequestId(dispatch_request_id),
        accepted_at: Instant(accepted_at),
    };
    validate_stored_lease(&authority.lease)?;
    validate_id(&authority.worker_session_id.0, "wsn_", "workerSessionId")?;
    validate_id(
        &authority.dispatch_request_id.0,
        "req_",
        "dispatchRequestId",
    )?;
    validate_instant(&authority.accepted_at, "acceptedAt")?;
    if authority.accepted_at.0 < authority.lease.issued_at.0
        || authority.accepted_at.0 >= authority.lease.expires_at.0
    {
        return Err(StorageError::adapter(
            "stored dispatch acceptance falls outside its lease window",
        ));
    }
    let current = load_lease_in_transaction(connection, job_id)?
        .ok_or_else(|| StorageError::adapter("dispatch authority lost its lease"))?;
    if current != authority.lease {
        return Err(StorageError::adapter(
            "dispatch authority differs from the current lease",
        ));
    }
    Ok(Some(authority))
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
    connection: &rusqlite::Connection,
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
