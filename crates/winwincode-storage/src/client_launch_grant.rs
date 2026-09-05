// SPDX-License-Identifier: Apache-2.0

//! Durable Server-side `WorkerLaunchGrant` ledger and its launch audit trail.
//!
//! The Control Plane is the authoritative owner of Worker launch grants (plan
//! 7.8, 14.3, 17.2): exactly one grant backs one `WorkerSession`, it binds
//! every identity the launch depends on (client node and device instance,
//! occupancy lease and fencing token, repository binding, worker session,
//! worker, and worker instance), and any field inconsistency refuses the
//! settlement. A grant is created `issued` after the issue gate validated the
//! caller is the occupancy lease holder, the lease is `occupied` or
//! `draining`, the binding belongs to the leased client and is visible to the
//! holder (an active `use` client access grant plus an active repository
//! access grant), and the client still has a free worker-session slot. The
//! Device Client's `client.worker.launch_ack` settles the grant exactly once:
//! an accepted acknowledgement consumes it, a rejection keeps it `issued`
//! with the rejection reason recorded in the launch audit trail, and a replay
//! of an already consumed grant is an accepted idempotent no-op.
//!
//! Uniqueness: at most one non-terminal (`issued` or `consumed`) grant may
//! exist per `worker_session_id` — the partial unique index is the durable
//! backstop behind the issue-time conflict check, so one worker session can
//! never carry two live worker launches (plan 14, one `WorkerSession` one
//! `Worker`).
//!
//! Credentials: only the `sha256:` digest of the one-time worker credential
//! is stored. The raw 32-byte material never enters this ledger; it crosses
//! the launch response and the device chain once and is dropped.
//!
//! Audit: every issuance, consumption, rejection, revocation, and expiry is
//! appended to `worker_launch_grant_audit` inside the same transaction as the
//! state change. Audit identities are derived deterministically from the
//! grant suffix and the action, so each of the at-most-once transitions
//! records exactly one row without the storage layer drawing entropy.

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;
const MAX_REASON_BYTES: usize = 256;

const WORKER_LAUNCH_GRANT_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS worker_launch_grants (
    worker_launch_grant_id TEXT PRIMARY KEY NOT NULL,
    client_node_id TEXT NOT NULL,
    client_instance_id TEXT NOT NULL,
    holder_user_id TEXT NOT NULL,
    occupancy_lease_id TEXT NOT NULL,
    occupancy_fencing_token INTEGER NOT NULL
        CHECK (occupancy_fencing_token > 0 AND occupancy_fencing_token <= 9007199254740991),
    repository_binding_id TEXT NOT NULL,
    worker_session_id TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    credential_digest TEXT NOT NULL
        CHECK (length(credential_digest) = 71 AND credential_digest LIKE 'sha256:%'),
    product_session_id TEXT,
    stage_run_id TEXT,
    expires_at TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('issued', 'consumed', 'revoked', 'expired')),
    consumed_at TEXT,
    ended_at TEXT,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT,
    FOREIGN KEY (repository_binding_id)
        REFERENCES repository_bindings(repository_binding_id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX IF NOT EXISTS worker_launch_grants_one_active_per_session
    ON worker_launch_grants (worker_session_id)
    WHERE state IN ('issued', 'consumed');
CREATE INDEX IF NOT EXISTS worker_launch_grants_by_client
    ON worker_launch_grants (client_node_id, state);
CREATE INDEX IF NOT EXISTS worker_launch_grants_by_lease
    ON worker_launch_grants (occupancy_lease_id, state);
CREATE TABLE IF NOT EXISTS worker_launch_grant_audit (
    audit_id TEXT PRIMARY KEY NOT NULL,
    action TEXT NOT NULL CHECK (action IN (
        'issued', 'consumed', 'launch_rejected', 'revoked', 'expired')),
    worker_launch_grant_id TEXT NOT NULL,
    client_node_id TEXT NOT NULL,
    holder_user_id TEXT NOT NULL,
    actor_user_id TEXT NOT NULL,
    reason TEXT,
    occurred_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS worker_launch_grant_audit_by_grant
    ON worker_launch_grant_audit (worker_launch_grant_id, occurred_at);
";

/// Lifecycle state of one `WorkerLaunchGrant` (plan 7.8).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerLaunchGrantState {
    /// Created and waiting for the Device Client launch acknowledgement.
    Issued,
    /// The Device Client accepted the launch and spawned the worker.
    Consumed,
    /// Terminal: the grant was revoked before the device accepted.
    Revoked,
    /// Terminal: the expiry deadline passed before the device accepted.
    Expired,
}

impl WorkerLaunchGrantState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issued => "issued",
            Self::Consumed => "consumed",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    fn parse(value: &str) -> Result<Self, WorkerLaunchGrantStoreError> {
        match value {
            "issued" => Ok(Self::Issued),
            "consumed" => Ok(Self::Consumed),
            "revoked" => Ok(Self::Revoked),
            "expired" => Ok(Self::Expired),
            _ => Err(error(
                WorkerLaunchGrantStoreErrorKind::CorruptState,
                "stored worker launch grant state is invalid",
            )),
        }
    }

    /// True while the grant still holds the worker session's single live
    /// launch slot.
    #[must_use]
    pub const fn is_non_terminal(self) -> bool {
        matches!(self, Self::Issued | Self::Consumed)
    }
}

impl fmt::Display for WorkerLaunchGrantState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Launch decision recorded in the launch audit trail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchAuditAction {
    /// The grant was issued after the gate passed.
    Issued,
    /// An accepted launch acknowledgement consumed the grant.
    Consumed,
    /// The device rejected the launch; the grant stayed `issued`.
    LaunchRejected,
    /// The grant was revoked before consumption.
    Revoked,
    /// The expiry sweep terminated the grant.
    Expired,
}

impl LaunchAuditAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issued => "issued",
            Self::Consumed => "consumed",
            Self::LaunchRejected => "launch_rejected",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    /// Two-character audit identity discriminator. Crockford Base32 digits
    /// only, so the derived audit id stays canonical.
    const fn code(self) -> &'static str {
        match self {
            Self::Issued => "01",
            Self::Consumed => "02",
            Self::LaunchRejected => "03",
            Self::Revoked => "04",
            Self::Expired => "05",
        }
    }
}

/// Validated atomic issue command (plan 14.3 step 4, 17.2).
///
/// The `_id` postfix on every field is the plan's own domain vocabulary, so
/// the lint against repeated field suffixes is intentionally allowed here.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchGrantIssuance {
    worker_launch_grant_id: String,
    client_node_id: String,
    client_instance_id: String,
    holder_user_id: String,
    occupancy_lease_id: String,
    occupancy_fencing_token: u64,
    repository_binding_id: String,
    worker_session_id: String,
    worker_id: String,
    worker_instance_id: String,
    credential_digest: String,
    product_session_id: Option<String>,
    stage_run_id: Option<String>,
    expires_at: Instant,
}

impl LaunchGrantIssuance {
    /// Builds one validated issue command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, a fencing token outside the durable
    /// range, a non-`sha256:` credential digest, or a non-canonical expiry
    /// before any durable write.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        worker_launch_grant_id: impl Into<String>,
        client_node_id: impl Into<String>,
        client_instance_id: impl Into<String>,
        holder_user_id: impl Into<String>,
        occupancy_lease_id: impl Into<String>,
        occupancy_fencing_token: u64,
        repository_binding_id: impl Into<String>,
        worker_session_id: impl Into<String>,
        worker_id: impl Into<String>,
        worker_instance_id: impl Into<String>,
        credential_digest: impl Into<String>,
        product_session_id: Option<String>,
        stage_run_id: Option<String>,
        expires_at: Instant,
    ) -> Result<Self, WorkerLaunchGrantStoreError> {
        let issuance = Self {
            worker_launch_grant_id: worker_launch_grant_id.into(),
            client_node_id: client_node_id.into(),
            client_instance_id: client_instance_id.into(),
            holder_user_id: holder_user_id.into(),
            occupancy_lease_id: occupancy_lease_id.into(),
            occupancy_fencing_token,
            repository_binding_id: repository_binding_id.into(),
            worker_session_id: worker_session_id.into(),
            worker_id: worker_id.into(),
            worker_instance_id: worker_instance_id.into(),
            credential_digest: credential_digest.into(),
            product_session_id,
            stage_run_id,
            expires_at,
        };
        validate_worker_launch_grant_id(&issuance.worker_launch_grant_id)?;
        validate_client_node_id(&issuance.client_node_id)?;
        validate_client_instance_id(&issuance.client_instance_id)?;
        validate_user_id(&issuance.holder_user_id)?;
        validate_occupancy_lease_id(&issuance.occupancy_lease_id)?;
        validate_fencing_token(issuance.occupancy_fencing_token)?;
        validate_repository_binding_id(&issuance.repository_binding_id)?;
        validate_worker_session_id(&issuance.worker_session_id)?;
        validate_worker_id(&issuance.worker_id)?;
        validate_worker_instance_id(&issuance.worker_instance_id)?;
        validate_credential_digest(&issuance.credential_digest)?;
        if let Some(product) = &issuance.product_session_id {
            validate_product_session_id(product)?;
        }
        if let Some(stage) = &issuance.stage_run_id {
            validate_stage_run_id(stage)?;
        }
        validate_instant(&issuance.expires_at, "grant expiry")?;
        Ok(issuance)
    }

    #[must_use]
    pub fn worker_launch_grant_id(&self) -> &str {
        &self.worker_launch_grant_id
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn holder_user_id(&self) -> &str {
        &self.holder_user_id
    }

    #[must_use]
    pub fn occupancy_lease_id(&self) -> &str {
        &self.occupancy_lease_id
    }

    #[must_use]
    pub const fn occupancy_fencing_token(&self) -> u64 {
        self.occupancy_fencing_token
    }

    #[must_use]
    pub fn repository_binding_id(&self) -> &str {
        &self.repository_binding_id
    }

    #[must_use]
    pub fn worker_session_id(&self) -> &str {
        &self.worker_session_id
    }
}

/// Validated `client.worker.launch_ack` settlement command (plan 14.3 step
/// 10, contract `client-control-port-v1.md`). Every bound field the device
/// echoes must match the grant exactly; any inconsistency refuses the
/// settlement without a state change (plan 17.2).
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchAckSettlement {
    worker_launch_grant_id: String,
    occupancy_lease_id: String,
    occupancy_fencing_token: u64,
    worker_session_id: String,
    worker_id: String,
    worker_instance_id: String,
    accepted: bool,
    rejection_reason: Option<String>,
}

impl LaunchAckSettlement {
    /// Builds one validated settlement command.
    ///
    /// The `_id` postfix on every field is the plan's own domain vocabulary,
    /// so the lint against repeated field suffixes is intentionally allowed;
    /// the wire payload simply carries this many identities.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, a fencing token outside the durable
    /// range, a missing rejection reason on a rejected acknowledgement, or an
    /// out-of-bounds reason text.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        worker_launch_grant_id: impl Into<String>,
        occupancy_lease_id: impl Into<String>,
        occupancy_fencing_token: u64,
        worker_session_id: impl Into<String>,
        worker_id: impl Into<String>,
        worker_instance_id: impl Into<String>,
        accepted: bool,
        rejection_reason: Option<String>,
    ) -> Result<Self, WorkerLaunchGrantStoreError> {
        let settlement = Self {
            worker_launch_grant_id: worker_launch_grant_id.into(),
            occupancy_lease_id: occupancy_lease_id.into(),
            occupancy_fencing_token,
            worker_session_id: worker_session_id.into(),
            worker_id: worker_id.into(),
            worker_instance_id: worker_instance_id.into(),
            accepted,
            rejection_reason,
        };
        validate_worker_launch_grant_id(&settlement.worker_launch_grant_id)?;
        validate_occupancy_lease_id(&settlement.occupancy_lease_id)?;
        validate_fencing_token(settlement.occupancy_fencing_token)?;
        validate_worker_session_id(&settlement.worker_session_id)?;
        validate_worker_id(&settlement.worker_id)?;
        validate_worker_instance_id(&settlement.worker_instance_id)?;
        if let Some(reason) = &settlement.rejection_reason {
            if settlement.accepted {
                return Err(error(
                    WorkerLaunchGrantStoreErrorKind::InvalidInput,
                    "an accepted launch acknowledgement carries no rejection reason",
                ));
            }
            if reason.is_empty() || reason.len() > MAX_REASON_BYTES {
                return Err(error(
                    WorkerLaunchGrantStoreErrorKind::InvalidInput,
                    "launch rejection reason must contain 1 to 256 bytes",
                ));
            }
        } else if !settlement.accepted {
            return Err(error(
                WorkerLaunchGrantStoreErrorKind::InvalidInput,
                "a rejected launch acknowledgement carries a rejection reason",
            ));
        }
        Ok(settlement)
    }

    #[must_use]
    pub fn worker_launch_grant_id(&self) -> &str {
        &self.worker_launch_grant_id
    }
}

/// Outcome of one settled `client.worker.launch_ack`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchAckOutcome {
    /// This acknowledgement consumed the grant: `issued -> consumed`.
    Consumed(Box<WorkerLaunchGrantRecord>),
    /// The grant was already consumed by an earlier acknowledgement; the
    /// replay is an accepted idempotent no-op.
    AlreadyConsumed,
    /// The device rejected the launch: the grant stays `issued` and the
    /// rejection reason landed in the launch audit trail.
    KeptIssued(Box<WorkerLaunchGrantRecord>),
}

/// Durable `WorkerLaunchGrant` row (plan 7.8).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerLaunchGrantRecord {
    /// Stable grant identifier.
    pub worker_launch_grant_id: String,
    /// Client node the worker launches on.
    pub client_node_id: String,
    /// Device instance identity the launch is bound to.
    pub client_instance_id: String,
    /// Occupancy holder the grant belongs to.
    pub holder_user_id: String,
    /// Occupancy lease authorizing the launch.
    pub occupancy_lease_id: String,
    /// Fencing token of the occupancy lease at issue time.
    pub occupancy_fencing_token: u64,
    /// Repository the worker runs against.
    pub repository_binding_id: String,
    /// Worker session the grant backs; one live grant per session.
    pub worker_session_id: String,
    /// Worker identity.
    pub worker_id: String,
    /// Worker instance identity.
    pub worker_instance_id: String,
    /// `sha256:` digest of the one-time worker credential; never the
    /// material itself.
    pub credential_digest: String,
    /// Product session the launch belongs to, when known.
    pub product_session_id: Option<String>,
    /// Stage run the launch belongs to, when known.
    pub stage_run_id: Option<String>,
    /// Deadline after which the grant may no longer be consumed.
    pub expires_at: Instant,
    /// Lifecycle state.
    pub state: WorkerLaunchGrantState,
    /// Consumption instant; set exactly on `issued -> consumed`.
    pub consumed_at: Option<Instant>,
    /// Terminal instant; set on `revoked` and `expired`.
    pub ended_at: Option<Instant>,
    /// Instant the grant was issued.
    pub created_at: Instant,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Stable launch-grant ledger failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerLaunchGrantStoreErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// The client node presence is not `online`.
    PresenceNotOnline,
    /// The client node is `locked`.
    ClientLocked,
    /// No occupancy lease matches the requested identity.
    UnknownOccupancyLease,
    /// The lease belongs to a user other than the grant holder.
    NotLeaseHolder,
    /// The lease is neither `occupied` nor `draining`.
    OccupancyNotConfirmed,
    /// The command carried a fencing token other than the bound token.
    FencingTokenMismatch,
    /// No repository binding matches the requested identity.
    UnknownRepositoryBinding,
    /// The binding belongs to a client node other than the leased one.
    BindingForeignClient,
    /// The holder has no visible binding (missing `use` client grant or
    /// missing active repository grant).
    BindingNotVisible,
    /// The client node has no free worker-session slot.
    CapacityExhausted,
    /// The worker session already carries a non-terminal grant.
    LaunchGrantConflict,
    /// The grant's expiry deadline passed before the acknowledgement.
    GrantExpired,
    /// No launch grant matches the requested identity.
    UnknownLaunchGrant,
    /// An echoed settlement field does not match the grant binding.
    FieldMismatch,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost a race that should be impossible inside
    /// one immediate transaction.
    RevisionConflict,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free launch-grant ledger error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerLaunchGrantStoreError {
    kind: WorkerLaunchGrantStoreErrorKind,
    message: String,
}

impl WorkerLaunchGrantStoreError {
    #[must_use]
    pub const fn kind(&self) -> WorkerLaunchGrantStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for WorkerLaunchGrantStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WorkerLaunchGrantStoreError {}

/// Worker launch grant ledger borrowing the sole product-state `SQLite`
/// authority.
pub struct WorkerLaunchGrantLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable worker launch grant ledger on this same product-state
    /// database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
    pub fn worker_launch_grant_ledger(
        &mut self,
    ) -> Result<WorkerLaunchGrantLedger<'_>, WorkerLaunchGrantStoreError> {
        WorkerLaunchGrantLedger::new(self)
    }
}

impl<'storage> WorkerLaunchGrantLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, WorkerLaunchGrantStoreError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(WORKER_LAUNCH_GRANT_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Atomically issues one worker launch grant (plan 14.3 step 4).
    ///
    /// Inside one immediate transaction the gate validates, in order: the
    /// client node is `online` and unlocked, the named occupancy lease exists
    /// on that node, is `occupied` or `draining`, belongs to the holder, and
    /// carries exactly the stamped fencing token; the repository binding
    /// exists, belongs to the leased node, and is visible to the holder (an
    /// active `use` client access grant plus an active repository access
    /// grant); the node still has a free worker-session slot counting its
    /// non-terminal grants (plan 14.5 durable reservation view); and the
    /// worker session carries no other non-terminal grant. On success the
    /// `issued` grant and its `issued` audit row commit together.
    ///
    /// # Errors
    ///
    /// Rejects any failed gate condition, a reused grant id, or storage
    /// failure.
    #[allow(clippy::too_many_lines)]
    pub fn issue(
        &mut self,
        issuance: &LaunchGrantIssuance,
        now: &Instant,
    ) -> Result<WorkerLaunchGrantRecord, WorkerLaunchGrantStoreError> {
        validate_instant(now, "issue time")?;
        let transaction = self.transaction()?;
        ensure_issue_gate(&transaction, issuance)?;
        let inserted = transaction
            .execute(
                "INSERT INTO worker_launch_grants
                 (worker_launch_grant_id, client_node_id, client_instance_id,
                  holder_user_id, occupancy_lease_id, occupancy_fencing_token,
                  repository_binding_id, worker_session_id, worker_id,
                  worker_instance_id, credential_digest, product_session_id,
                  stage_run_id, expires_at, state, consumed_at, ended_at,
                  created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, 'issued', NULL, NULL, ?15, 1)",
                params![
                    issuance.worker_launch_grant_id,
                    issuance.client_node_id,
                    issuance.client_instance_id,
                    issuance.holder_user_id,
                    issuance.occupancy_lease_id,
                    sql_integer(issuance.occupancy_fencing_token)?,
                    issuance.repository_binding_id,
                    issuance.worker_session_id,
                    issuance.worker_id,
                    issuance.worker_instance_id,
                    issuance.credential_digest,
                    issuance.product_session_id,
                    issuance.stage_run_id,
                    issuance.expires_at.0,
                    now.0,
                ],
            )
            .map_err(|sql| map_grant_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                WorkerLaunchGrantStoreErrorKind::Storage,
                "worker launch grant insert did not store exactly one row",
            ));
        }
        insert_audit(
            &transaction,
            LaunchAuditAction::Issued,
            &issuance.worker_launch_grant_id,
            &issuance.client_node_id,
            &issuance.holder_user_id,
            &issuance.holder_user_id,
            None,
            now,
        )?;
        let record = load_launch_grant(&transaction, &issuance.worker_launch_grant_id)?
            .ok_or_else(grant_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Settles one `client.worker.launch_ack` (plan 14.3 step 10).
    ///
    /// The bound lease, fencing token, worker session, worker, and worker
    /// instance echoed by the device must all match the grant; any mismatch
    /// refuses the settlement with no state change. An accepted
    /// acknowledgement consumes an `issued` grant exactly once (its expiry
    /// deadline must still be open); a replay against an already `consumed`
    /// grant is an accepted idempotent no-op. A rejected acknowledgement
    /// keeps the grant `issued` and records the rejection reason in the
    /// launch audit trail.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant, a field mismatch, a stale token, an expired
    /// grant, an illegal transition, or storage failure.
    #[allow(clippy::too_many_lines)]
    pub fn settle_launch_ack(
        &mut self,
        settlement: &LaunchAckSettlement,
        now: &Instant,
    ) -> Result<LaunchAckOutcome, WorkerLaunchGrantStoreError> {
        validate_instant(now, "launch acknowledgement time")?;
        let transaction = self.transaction()?;
        let record = require_launch_grant(&transaction, &settlement.worker_launch_grant_id)?;
        ensure_settlement_matches(&record, settlement)?;
        if !settlement.accepted {
            if record.state != WorkerLaunchGrantState::Issued {
                return Err(illegal_transition(&record, "launch rejection"));
            }
            insert_audit(
                &transaction,
                LaunchAuditAction::LaunchRejected,
                &record.worker_launch_grant_id,
                &record.client_node_id,
                &record.holder_user_id,
                &record.holder_user_id,
                settlement.rejection_reason.as_deref(),
                now,
            )?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            Ok(LaunchAckOutcome::KeptIssued(Box::new(record)))
        } else if record.state == WorkerLaunchGrantState::Consumed {
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            Ok(LaunchAckOutcome::AlreadyConsumed)
        } else {
            if record.state != WorkerLaunchGrantState::Issued {
                return Err(illegal_transition(&record, "launch acknowledgement"));
            }
            if now.0.as_str() >= record.expires_at.0.as_str() {
                return Err(error(
                    WorkerLaunchGrantStoreErrorKind::GrantExpired,
                    "the launch grant expired before the device accepted",
                ));
            }
            let updated = transaction
                .execute(
                    "UPDATE worker_launch_grants
                     SET state = 'consumed', consumed_at = ?2, revision = revision + 1
                     WHERE worker_launch_grant_id = ?1 AND state = 'issued'",
                    params![settlement.worker_launch_grant_id, now.0],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated != 1 {
                return Err(cas_lost("launch acknowledgement"));
            }
            insert_audit(
                &transaction,
                LaunchAuditAction::Consumed,
                &record.worker_launch_grant_id,
                &record.client_node_id,
                &record.holder_user_id,
                &record.holder_user_id,
                None,
                now,
            )?;
            let consumed = require_launch_grant(&transaction, &settlement.worker_launch_grant_id)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            Ok(LaunchAckOutcome::Consumed(Box::new(consumed)))
        }
    }

    /// Revokes an `issued` grant before the device accepted it (plan 18
    /// recovery family). A consumed grant is a live worker and leaves
    /// through the stop flow, not through revocation.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant, a non-`issued` grant, an over-long reason,
    /// or storage failure.
    pub fn revoke(
        &mut self,
        worker_launch_grant_id: &str,
        actor_user_id: &str,
        reason: Option<&str>,
        now: &Instant,
    ) -> Result<WorkerLaunchGrantRecord, WorkerLaunchGrantStoreError> {
        validate_worker_launch_grant_id(worker_launch_grant_id)?;
        validate_user_id(actor_user_id)?;
        if let Some(reason) = reason
            && (reason.is_empty() || reason.len() > MAX_REASON_BYTES)
        {
            return Err(error(
                WorkerLaunchGrantStoreErrorKind::InvalidInput,
                "revocation reason must contain 1 to 256 bytes",
            ));
        }
        validate_instant(now, "revocation time")?;
        let transaction = self.transaction()?;
        let record = require_launch_grant(&transaction, worker_launch_grant_id)?;
        if record.state != WorkerLaunchGrantState::Issued {
            return Err(illegal_transition(&record, "revocation"));
        }
        let updated = transaction
            .execute(
                "UPDATE worker_launch_grants
                 SET state = 'revoked', ended_at = ?2, revision = revision + 1
                 WHERE worker_launch_grant_id = ?1 AND state = 'issued'",
                params![worker_launch_grant_id, now.0],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("revocation"));
        }
        insert_audit(
            &transaction,
            LaunchAuditAction::Revoked,
            &record.worker_launch_grant_id,
            &record.client_node_id,
            &record.holder_user_id,
            actor_user_id,
            reason,
            now,
        )?;
        let revoked = require_launch_grant(&transaction, worker_launch_grant_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(revoked)
    }

    /// Expires every `issued` grant whose expiry deadline is at or before
    /// `cutoff` (plan 7.8 `expired`). Returns the expired grant ids.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire(&mut self, cutoff: &Instant) -> Result<Vec<String>, WorkerLaunchGrantStoreError> {
        validate_instant(cutoff, "expiry cutoff")?;
        let transaction = self.transaction()?;
        let mut statement = transaction
            .prepare(
                "SELECT worker_launch_grant_id FROM worker_launch_grants
                 WHERE state = 'issued' AND expires_at <= ?1",
            )
            .map_err(|sql| sql_error(&sql))?;
        let candidates = statement
            .query_map([cutoff.0.as_str()], |row| row.get::<_, String>(0))
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        let mut expired = Vec::with_capacity(candidates.len());
        for worker_launch_grant_id in candidates {
            let record = require_launch_grant(&transaction, &worker_launch_grant_id)?;
            let updated = transaction
                .execute(
                    "UPDATE worker_launch_grants
                     SET state = 'expired', ended_at = ?2, revision = revision + 1
                     WHERE worker_launch_grant_id = ?1 AND state = 'issued'",
                    params![worker_launch_grant_id, cutoff.0],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated != 1 {
                continue;
            }
            insert_audit(
                &transaction,
                LaunchAuditAction::Expired,
                &record.worker_launch_grant_id,
                &record.client_node_id,
                &record.holder_user_id,
                &record.holder_user_id,
                Some("expiry deadline passed"),
                cutoff,
            )?;
            expired.push(worker_launch_grant_id);
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(expired)
    }

    /// Returns one durable launch grant projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical grant identity, corrupt stored rows, or
    /// storage failure.
    pub fn snapshot(
        &self,
        worker_launch_grant_id: &str,
    ) -> Result<Option<WorkerLaunchGrantRecord>, WorkerLaunchGrantStoreError> {
        validate_worker_launch_grant_id(worker_launch_grant_id)?;
        load_launch_grant(self.connection()?, worker_launch_grant_id)
    }

    /// Returns the one non-terminal grant of a worker session, if any.
    ///
    /// More than one non-terminal row is a corrupt database and fails closed.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical worker session identity, a corrupt active
    /// set, or storage failure.
    pub fn active_grant_for_session(
        &self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerLaunchGrantRecord>, WorkerLaunchGrantStoreError> {
        validate_worker_session_id(worker_session_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT worker_launch_grant_id FROM worker_launch_grants
                 WHERE worker_session_id = ?1 AND state IN ('issued', 'consumed')",
            )
            .map_err(|sql| sql_error(&sql))?;
        let ids = statement
            .query_map([worker_session_id], |row| row.get::<_, String>(0))
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        match ids.len() {
            0 => Ok(None),
            1 => load_launch_grant(connection, &ids[0]),
            _ => Err(error(
                WorkerLaunchGrantStoreErrorKind::CorruptState,
                "worker session holds more than one non-terminal launch grant",
            )),
        }
    }

    /// Counts the non-terminal (`issued` plus `consumed`) grants of one
    /// client node — the durable reservation view capacity is judged against
    /// (plan 14.5).
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or storage failure.
    pub fn non_terminal_count_for_node(
        &self,
        client_node_id: &str,
    ) -> Result<u64, WorkerLaunchGrantStoreError> {
        validate_client_node_id(client_node_id)?;
        let connection = self.connection()?;
        let stored: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM worker_launch_grants
                 WHERE client_node_id = ?1 AND state IN ('issued', 'consumed')",
                [client_node_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|sql| sql_error(&sql))?;
        from_sql_integer(stored, "non-terminal launch grant count")
    }

    /// Returns every durable launch audit entry of one grant, oldest first.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical grant identity or storage failure.
    pub fn audit_trail(
        &self,
        worker_launch_grant_id: &str,
    ) -> Result<Vec<LaunchAuditEntry>, WorkerLaunchGrantStoreError> {
        validate_worker_launch_grant_id(worker_launch_grant_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT audit_id, action, worker_launch_grant_id, client_node_id,
                        holder_user_id, actor_user_id, reason, occurred_at
                 FROM worker_launch_grant_audit
                 WHERE worker_launch_grant_id = ?1
                 ORDER BY occurred_at, audit_id",
            )
            .map_err(|sql| sql_error(&sql))?;
        let entries = statement
            .query_map([worker_launch_grant_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        entries
            .into_iter()
            .map(LaunchAuditEntry::from_row)
            .collect()
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, WorkerLaunchGrantStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&rusqlite::Connection, WorkerLaunchGrantStoreError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

/// One launch audit row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchAuditEntry {
    /// Stable audit identity.
    pub audit_id: String,
    /// Recorded action.
    pub action: LaunchAuditAction,
    /// Grant the entry refers to.
    pub worker_launch_grant_id: String,
    /// Client node the grant launches on.
    pub client_node_id: String,
    /// Holder the grant belongs to.
    pub holder_user_id: String,
    /// User that drove the transition.
    pub actor_user_id: String,
    /// Machine-readable reason, when one exists.
    pub reason: Option<String>,
    /// Instant the entry was recorded.
    pub occurred_at: Instant,
}

impl LaunchAuditEntry {
    #[allow(clippy::type_complexity)]
    fn from_row(
        row: (
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
        ),
    ) -> Result<Self, WorkerLaunchGrantStoreError> {
        let (
            audit_id,
            action,
            worker_launch_grant_id,
            client_node_id,
            holder_user_id,
            actor_user_id,
            reason,
            occurred_at,
        ) = row;
        let action = match action.as_str() {
            "issued" => LaunchAuditAction::Issued,
            "consumed" => LaunchAuditAction::Consumed,
            "launch_rejected" => LaunchAuditAction::LaunchRejected,
            "revoked" => LaunchAuditAction::Revoked,
            "expired" => LaunchAuditAction::Expired,
            _ => {
                return Err(error(
                    WorkerLaunchGrantStoreErrorKind::CorruptState,
                    "stored launch audit action is invalid",
                ));
            }
        };
        Ok(Self {
            audit_id,
            action,
            worker_launch_grant_id,
            client_node_id,
            holder_user_id,
            actor_user_id,
            reason,
            occurred_at: parse_stored_instant(&occurred_at, "launch audit instant")?,
        })
    }
}

/// Applies the issue gate inside the caller's transaction (plan 14.3 step 3,
/// 17.2). Conditions are judged in the plan order: device facts, occupancy,
/// binding visibility, capacity, and the one-live-grant-per-session rule.
fn ensure_issue_gate(
    transaction: &Transaction<'_>,
    issuance: &LaunchGrantIssuance,
) -> Result<(), WorkerLaunchGrantStoreError> {
    let max_slots = ensure_device_facts(transaction, issuance)?;
    ensure_occupancy_facts(transaction, issuance)?;
    ensure_binding_visibility(transaction, issuance)?;
    ensure_capacity_and_session_uniqueness(transaction, issuance, max_slots)?;
    Ok(())
}

/// Judges the device facts: the client node exists, is `online`, is
/// unlocked, and returns its worker-session capacity.
fn ensure_device_facts(
    transaction: &Transaction<'_>,
    issuance: &LaunchGrantIssuance,
) -> Result<u64, WorkerLaunchGrantStoreError> {
    let node = transaction
        .query_row(
            "SELECT presence_state, lock_state, max_concurrent_worker_sessions
             FROM client_nodes WHERE client_node_id = ?1",
            [issuance.client_node_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    let Some((presence, lock_state, max_slots)) = node else {
        return Err(unknown_client_node());
    };
    if lock_state == "locked" {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::ClientLocked,
            "client node is locked",
        ));
    }
    if presence != "online" {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::PresenceNotOnline,
            "client node presence is not online",
        ));
    }
    from_sql_integer(max_slots, "client worker session capacity")
}

/// Judges the occupancy facts (plan 17.2): the lease exists on the leased
/// node, is confirmed by the device (`occupied` or `draining`), belongs to
/// the holder, and carries the exact stamped fencing token.
fn ensure_occupancy_facts(
    transaction: &Transaction<'_>,
    issuance: &LaunchGrantIssuance,
) -> Result<(), WorkerLaunchGrantStoreError> {
    let lease = transaction
        .query_row(
            "SELECT client_node_id, holder_user_id, fencing_token, state
             FROM client_occupancy_leases WHERE occupancy_lease_id = ?1",
            [issuance.occupancy_lease_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    let Some((lease_node, lease_holder, lease_token, lease_state)) = lease else {
        return Err(unknown_occupancy_lease());
    };
    if lease_node != issuance.client_node_id {
        return Err(unknown_occupancy_lease());
    }
    if lease_holder != issuance.holder_user_id {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::NotLeaseHolder,
            "the occupancy lease belongs to another user",
        ));
    }
    if lease_state != "occupied" && lease_state != "draining" {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::OccupancyNotConfirmed,
            "the occupancy lease is not confirmed by the device",
        ));
    }
    if from_sql_integer(lease_token, "lease fencing token")? != issuance.occupancy_fencing_token {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::FencingTokenMismatch,
            "the stamped occupancy fencing token does not match the lease",
        ));
    }
    Ok(())
}

/// Judges the binding facts (plan 13.4): the binding exists, belongs to the
/// leased node, and is visible to the holder — an active `use` client access
/// grant plus an active repository access grant.
fn ensure_binding_visibility(
    transaction: &Transaction<'_>,
    issuance: &LaunchGrantIssuance,
) -> Result<(), WorkerLaunchGrantStoreError> {
    let binding = transaction
        .query_row(
            "SELECT client_node_id FROM repository_bindings
             WHERE repository_binding_id = ?1",
            [issuance.repository_binding_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    match binding.as_deref() {
        None => return Err(unknown_repository_binding()),
        Some(binding_node) if binding_node != issuance.client_node_id => {
            return Err(error(
                WorkerLaunchGrantStoreErrorKind::BindingForeignClient,
                "the repository binding belongs to another client node",
            ));
        }
        Some(_) => {}
    }
    let client_grant: Option<i64> = transaction
        .query_row(
            "SELECT 1 FROM client_access_grants
             WHERE client_node_id = ?1 AND user_id = ?2 AND state = 'active'
               AND permissions IN ('use', 'use+manage', 'use+share', 'use+manage+share')",
            params![issuance.client_node_id, issuance.holder_user_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    let repo_grant: Option<i64> = transaction
        .query_row(
            "SELECT 1 FROM repository_access_grants
             WHERE repository_binding_id = ?1 AND user_id = ?2 AND state = 'active'",
            params![issuance.repository_binding_id, issuance.holder_user_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    if client_grant.is_none() || repo_grant.is_none() {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::BindingNotVisible,
            "the repository binding is not visible to the holder",
        ));
    }
    Ok(())
}

/// Judges capacity (plan 14.5, no overselling) and the one-live-grant-per-
/// worker-session rule; the partial unique index is the durable backstop.
fn ensure_capacity_and_session_uniqueness(
    transaction: &Transaction<'_>,
    issuance: &LaunchGrantIssuance,
    max_slots: u64,
) -> Result<(), WorkerLaunchGrantStoreError> {
    let reserved: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM worker_launch_grants
             WHERE client_node_id = ?1 AND state IN ('issued', 'consumed')",
            [issuance.client_node_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))?;
    let reserved = from_sql_integer(reserved, "reserved worker session count")?;
    if reserved.saturating_add(1) > max_slots {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::CapacityExhausted,
            "client node has no free worker session slot",
        ));
    }
    let duplicate = transaction
        .query_row(
            "SELECT 1 FROM worker_launch_grants
             WHERE worker_session_id = ?1 AND state IN ('issued', 'consumed')",
            [issuance.worker_session_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    if duplicate.is_some() {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::LaunchGrantConflict,
            "the worker session already carries a non-terminal launch grant",
        ));
    }
    Ok(())
}

/// Validates that every echoed settlement field matches the grant binding
/// (plan 17.2: any field inconsistency refuses). The occupancy stamp is
/// judged first so a stale token reports precisely.
fn ensure_settlement_matches(
    record: &WorkerLaunchGrantRecord,
    settlement: &LaunchAckSettlement,
) -> Result<(), WorkerLaunchGrantStoreError> {
    if record.occupancy_lease_id != settlement.occupancy_lease_id
        || record.occupancy_fencing_token != settlement.occupancy_fencing_token
    {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::FencingTokenMismatch,
            "the launch acknowledgement carries a stale occupancy stamp",
        ));
    }
    let consistent = record.worker_session_id == settlement.worker_session_id
        && record.worker_id == settlement.worker_id
        && record.worker_instance_id == settlement.worker_instance_id;
    if consistent {
        Ok(())
    } else {
        Err(error(
            WorkerLaunchGrantStoreErrorKind::FieldMismatch,
            "the launch acknowledgement does not match the grant binding",
        ))
    }
}

/// Appends one launch audit row inside the caller's transaction. Audit
/// identities derive from the grant suffix and the action code, so each of
/// the at-most-once transitions records exactly one row.
#[allow(clippy::too_many_arguments)]
fn insert_audit(
    transaction: &Transaction<'_>,
    action: LaunchAuditAction,
    worker_launch_grant_id: &str,
    client_node_id: &str,
    holder_user_id: &str,
    actor_user_id: &str,
    reason: Option<&str>,
    now: &Instant,
) -> Result<(), WorkerLaunchGrantStoreError> {
    let audit_id = audit_identity(action, worker_launch_grant_id)?;
    let inserted = transaction
        .execute(
            "INSERT INTO worker_launch_grant_audit
             (audit_id, action, worker_launch_grant_id, client_node_id,
              holder_user_id, actor_user_id, reason, occurred_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                audit_id,
                action.as_str(),
                worker_launch_grant_id,
                client_node_id,
                holder_user_id,
                actor_user_id,
                reason,
                now.0,
            ],
        )
        .map_err(|sql| sql_error(&sql))?;
    if inserted != 1 {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::Storage,
            "launch audit insert did not store exactly one row",
        ));
    }
    Ok(())
}

/// Deterministic audit identity: the action discriminator followed by the
/// first 24 characters of the grant suffix. Every action happens at most
/// once per grant, so identities never collide.
fn audit_identity(
    action: LaunchAuditAction,
    worker_launch_grant_id: &str,
) -> Result<String, WorkerLaunchGrantStoreError> {
    let suffix = worker_launch_grant_id.strip_prefix("wlg_").ok_or_else(|| {
        error(
            WorkerLaunchGrantStoreErrorKind::InvalidInput,
            "worker launch grant id is not canonical",
        )
    })?;
    let mut identity = String::with_capacity(4 + 26);
    identity.push_str("wla_");
    identity.push_str(action.code());
    identity.push_str(&suffix[..24]);
    Ok(identity)
}

fn load_launch_grant(
    connection: &rusqlite::Connection,
    worker_launch_grant_id: &str,
) -> Result<Option<WorkerLaunchGrantRecord>, WorkerLaunchGrantStoreError> {
    connection
        .query_row(
            "SELECT worker_launch_grant_id, client_node_id, client_instance_id,
                    holder_user_id, occupancy_lease_id, occupancy_fencing_token,
                    repository_binding_id, worker_session_id, worker_id,
                    worker_instance_id, credential_digest, product_session_id,
                    stage_run_id, expires_at, state, consumed_at, ended_at,
                    created_at, revision
             FROM worker_launch_grants WHERE worker_launch_grant_id = ?1",
            [worker_launch_grant_id],
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
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, String>(17)?,
                    row.get::<_, i64>(18)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(launch_grant_record_from_row)
        .transpose()
}

#[allow(clippy::type_complexity)]
fn launch_grant_record_from_row(
    row: (
        String,
        String,
        String,
        String,
        String,
        i64,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        i64,
    ),
) -> Result<WorkerLaunchGrantRecord, WorkerLaunchGrantStoreError> {
    let (
        worker_launch_grant_id,
        client_node_id,
        client_instance_id,
        holder_user_id,
        occupancy_lease_id,
        occupancy_fencing_token,
        repository_binding_id,
        worker_session_id,
        worker_id,
        worker_instance_id,
        credential_digest,
        product_session_id,
        stage_run_id,
        expires_at,
        state,
        consumed_at,
        ended_at,
        created_at,
        revision,
    ) = row;
    let parse_optional = |value: Option<String>,
                          label: &'static str|
     -> Result<Option<Instant>, WorkerLaunchGrantStoreError> {
        value
            .map(|value| parse_stored_instant(&value, label))
            .transpose()
    };
    Ok(WorkerLaunchGrantRecord {
        worker_launch_grant_id,
        client_node_id,
        client_instance_id,
        holder_user_id,
        occupancy_lease_id,
        occupancy_fencing_token: from_sql_integer(
            occupancy_fencing_token,
            "occupancy fencing token",
        )?,
        repository_binding_id,
        worker_session_id,
        worker_id,
        worker_instance_id,
        credential_digest,
        product_session_id,
        stage_run_id,
        expires_at: parse_stored_instant(&expires_at, "grant expiry")?,
        state: WorkerLaunchGrantState::parse(&state)?,
        consumed_at: parse_optional(consumed_at, "consumed at")?,
        ended_at: parse_optional(ended_at, "ended at")?,
        created_at: parse_stored_instant(&created_at, "created at")?,
        revision: from_sql_integer(revision, "launch grant revision")?,
    })
}

fn validate_schema(connection: &rusqlite::Connection) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_columns(
        connection,
        "worker_launch_grants",
        &[
            "worker_launch_grant_id",
            "client_node_id",
            "client_instance_id",
            "holder_user_id",
            "occupancy_lease_id",
            "occupancy_fencing_token",
            "repository_binding_id",
            "worker_session_id",
            "worker_id",
            "worker_instance_id",
            "credential_digest",
            "product_session_id",
            "stage_run_id",
            "expires_at",
            "state",
            "consumed_at",
            "ended_at",
            "created_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "worker_launch_grant_audit",
        &[
            "audit_id",
            "action",
            "worker_launch_grant_id",
            "client_node_id",
            "holder_user_id",
            "actor_user_id",
            "reason",
            "occurred_at",
        ],
    )
}

fn validate_columns(
    connection: &rusqlite::Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), WorkerLaunchGrantStoreError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::CorruptState,
            "worker launch grant ledger schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), WorkerLaunchGrantStoreError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(invalid_id(label));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(invalid_id(label));
    }
    Ok(())
}

fn invalid_id(label: &str) -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::InvalidInput,
        format!("{label} is not canonical"),
    )
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9'
            | b'A'..=b'H'
            | b'J'
            | b'K'
            | b'M'
            | b'N'
            | b'P'..=b'T'
            | b'V'..=b'Z'
    )
}

fn validate_worker_launch_grant_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "wlg_", "worker launch grant id")
}

fn validate_client_node_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "cnd_", "client node id")
}

fn validate_client_instance_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "cix_", "client instance id")
}

fn validate_user_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "usr_", "user id")
}

fn validate_occupancy_lease_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "ocl_", "occupancy lease id")
}

fn validate_repository_binding_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "rbd_", "repository binding id")
}

fn validate_worker_session_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "ws_", "worker session id")
}

fn validate_worker_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "wkr_", "worker id")
}

fn validate_worker_instance_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "winst_", "worker instance id")
}

fn validate_product_session_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "ps_", "product session id")
}

fn validate_stage_run_id(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    validate_crockford_id(value, "run_", "stage run id")
}

/// Validates the canonical `sha256:` + 64 lowercase hex digest shape.
fn validate_credential_digest(value: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::InvalidInput,
            "credential digest is not a sha256 digest",
        ));
    };
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(error(
            WorkerLaunchGrantStoreErrorKind::InvalidInput,
            "credential digest is not a lowercase sha256 digest",
        ))
    }
}

/// Validates the canonical `domain.Instant` shape (`YYYY-MM-DDTHH:MM:SS.sssZ`).
fn validate_instant(value: &Instant, label: &str) -> Result<(), WorkerLaunchGrantStoreError> {
    let bytes = value.0.as_bytes();
    let punctuation = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
    ];
    let valid = bytes.len() == 24
        && bytes[23] == b'Z'
        && punctuation
            .iter()
            .all(|(index, byte)| bytes[*index] == *byte)
        && bytes.iter().enumerate().all(|(index, byte)| {
            punctuation.iter().any(|(at, _)| at == &index) || index == 23 || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(error(
            WorkerLaunchGrantStoreErrorKind::InvalidInput,
            format!("{label} instant is not canonical"),
        ))
    }
}

fn parse_stored_instant(value: &str, label: &str) -> Result<Instant, WorkerLaunchGrantStoreError> {
    let instant = Instant(value.to_owned());
    validate_instant(&instant, label).map(|()| instant)
}

fn validate_fencing_token(value: u64) -> Result<(), WorkerLaunchGrantStoreError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::InvalidInput,
            "fencing token is outside the durable range",
        ));
    }
    Ok(())
}

fn illegal_transition(
    record: &WorkerLaunchGrantRecord,
    action: &str,
) -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::IllegalStateTransition,
        format!(
            "worker launch grant transition {} during {action} is not legal",
            record.state
        ),
    )
}

fn map_grant_insert_sql(sql: &rusqlite::Error) -> WorkerLaunchGrantStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            // The realistic unique violation is the one-live-grant-per-
            // session partial index; a grant id reuse shares the extended
            // code family and fails closed as a conflict too.
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                WorkerLaunchGrantStoreErrorKind::LaunchGrantConflict,
                "the worker session already carries a non-terminal launch grant",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                WorkerLaunchGrantStoreErrorKind::LaunchGrantConflict,
                "worker launch grant id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => unknown_client_node(),
            _ => error(
                WorkerLaunchGrantStoreErrorKind::InvalidInput,
                "worker launch grant violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn grant_missing_after_write() -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::CorruptState,
        "worker launch grant row is missing after the write",
    )
}

/// Loads one grant or fails with the unknown-grant category.
fn require_launch_grant(
    connection: &rusqlite::Connection,
    worker_launch_grant_id: &str,
) -> Result<WorkerLaunchGrantRecord, WorkerLaunchGrantStoreError> {
    load_launch_grant(connection, worker_launch_grant_id)?.ok_or_else(|| {
        error(
            WorkerLaunchGrantStoreErrorKind::UnknownLaunchGrant,
            "worker launch grant does not exist",
        )
    })
}

fn unknown_client_node() -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::UnknownClientNode,
        "client node does not exist",
    )
}

fn unknown_occupancy_lease() -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::UnknownOccupancyLease,
        "occupancy lease does not exist for this launch",
    )
}

fn unknown_repository_binding() -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::UnknownRepositoryBinding,
        "repository binding does not exist",
    )
}

fn cas_lost(action: &str) -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::RevisionConflict,
        format!("worker launch grant compare-and-swap lost during {action}"),
    )
}

fn sql_integer(value: u64) -> Result<i64, WorkerLaunchGrantStoreError> {
    i64::try_from(value).map_err(|_| {
        error(
            WorkerLaunchGrantStoreErrorKind::InvalidInput,
            "numeric value exceeds the SQLite integer range",
        )
    })
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, WorkerLaunchGrantStoreError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            WorkerLaunchGrantStoreErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            WorkerLaunchGrantStoreErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn storage_error(storage: &StorageError) -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::Storage,
        format!("worker launch grant ledger storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> WorkerLaunchGrantStoreError {
    error(
        WorkerLaunchGrantStoreErrorKind::Storage,
        "worker launch grant ledger storage operation failed",
    )
}

fn error(
    kind: WorkerLaunchGrantStoreErrorKind,
    message: impl Into<String>,
) -> WorkerLaunchGrantStoreError {
    WorkerLaunchGrantStoreError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::AccessGrantIssuance;
    use crate::GrantPermissions;
    use crate::GrantSource;
    use crate::GrantTrustMode;
    use crate::OccupancyClaim;
    use crate::OccupancyLeaseState;
    use crate::RepositoryAccessGrantIssuance;
    use crate::RepositoryAvailability;
    use crate::RepositoryBindingLedger;
    use crate::RepositoryBindingProjection;
    use crate::RepositoryDirtyState;
    use crate::RepositoryGrantPermissions;

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-launch-grant-unit-{name}-{}-{suffix}-{nanos}",
            std::process::id()
        ))
    }

    fn unit_instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn crockford(seed: u64) -> String {
        const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut identity = String::with_capacity(26);
        let mut value = seed;
        for _ in 0..26 {
            let digit = usize::try_from(value % 32).expect("digit fits");
            identity.push(ALPHABET[digit] as char);
            value /= 32;
        }
        identity
    }

    fn grant_id(seed: u64) -> String {
        format!("wlg_{}", crockford(seed))
    }

    fn session_id(seed: u64) -> String {
        format!("ws_{}", crockford(seed))
    }

    fn user_id(seed: u64) -> String {
        format!("usr_{}", crockford(seed))
    }

    const DIGEST: &str = "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn seed_online_node(storage: &mut SqliteStorage, seed: u64) -> String {
        let client_node_id = format!("cnd_{}", crockford(seed));
        let registration = crate::ClientNodeRegistration::try_new(
            client_node_id.clone(),
            format!("{seed:010}"),
            format!("Device {seed}"),
            "aarch64-apple-darwin",
            "aarch64",
            "1.2.3",
            None,
            Some(format!("cix_{}", crockford(seed + 1))),
            4,
        )
        .expect("registration");
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .register(&registration, 0, &unit_instant("2026-01-01T00:00:00.000Z"))
            .expect("register");
        let online = registry
            .update_presence(
                client_node_id.as_str(),
                crate::ClientPresenceState::Online,
                1,
            )
            .expect("presence online");
        assert_eq!(online.presence_state, crate::ClientPresenceState::Online);
        client_node_id
    }

    /// Stages an active `use` client access grant for the user.
    fn stage_client_grant(storage: &mut SqliteStorage, node: &str, user: &str, seed: u64) {
        let issuance = AccessGrantIssuance::try_new(
            format!("cag_{}", crockford(seed)),
            node,
            user,
            user,
            GrantTrustMode::Trusted,
            None,
        )
        .expect("issuance");
        storage
            .client_connect_ledger()
            .expect("connect ledger")
            .create_grant(
                &issuance,
                GrantSource::Administrator,
                GrantPermissions::USE,
                &unit_instant("2026-01-01T00:00:00.000Z"),
            )
            .expect("grant");
    }

    /// Claims occupancy and walks the lease to `occupied` (or `draining`)
    /// through the real ledger.
    fn stage_occupied_lease(
        storage: &mut SqliteStorage,
        node: &str,
        holder: &str,
        seed: u64,
    ) -> (String, u64) {
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        let claim = OccupancyClaim::try_new(
            format!("ocl_{}", crockford(seed)),
            node,
            holder,
            format!("req_{}", crockford(seed + 1)),
        )
        .expect("claim");
        let lease = ledger
            .atomic_claim(&claim, &unit_instant("2026-01-01T00:01:00.000Z"))
            .expect("claim");
        let lease = ledger
            .record_acknowledgement(
                &lease.occupancy_lease_id,
                lease.fencing_token,
                None,
                &unit_instant("2026-01-01T00:01:01.000Z"),
            )
            .expect("ack");
        assert_eq!(lease.state, OccupancyLeaseState::Occupied);
        (lease.occupancy_lease_id, lease.fencing_token)
    }

    /// Stages one visible repository binding with an active `use` grant for
    /// the user.
    fn stage_visible_binding(
        storage: &mut SqliteStorage,
        node: &str,
        user: &str,
        seed: u64,
    ) -> String {
        let binding_id = format!("rbd_{}", crockford(seed));
        let mut ledger = repository_ledger(storage);
        let projection = RepositoryBindingProjection::try_new(
            binding_id.clone(),
            node,
            "winwincode",
            Some("main".to_owned()),
            Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            RepositoryDirtyState::Clean,
            RepositoryAvailability::Available,
            format!("sha256:{seed:064}"),
        )
        .expect("projection");
        ledger
            .upsert(
                &projection,
                None,
                0,
                &unit_instant("2026-01-01T00:00:30.000Z"),
            )
            .expect("upsert");
        let issuance = RepositoryAccessGrantIssuance::try_new(
            format!("rag_{}", crockford(seed + 1)),
            binding_id.clone(),
            user,
            user,
        )
        .expect("repo grant issuance");
        ledger
            .create_grant(
                &issuance,
                RepositoryGrantPermissions::Use,
                &unit_instant("2026-01-01T00:00:31.000Z"),
            )
            .expect("repo grant");
        binding_id
    }

    fn repository_ledger(storage: &mut SqliteStorage) -> RepositoryBindingLedger<'_> {
        storage.repository_binding_ledger().expect("binding ledger")
    }

    #[allow(clippy::too_many_arguments)]
    fn issuance(
        grant_seed: u64,
        node: &str,
        instance: &str,
        holder: &str,
        lease_id: &str,
        token: u64,
        binding: &str,
        session_seed: u64,
    ) -> LaunchGrantIssuance {
        LaunchGrantIssuance::try_new(
            grant_id(grant_seed),
            node,
            instance,
            holder,
            lease_id,
            token,
            binding,
            session_id(session_seed),
            format!("wkr_{}", crockford(session_seed + 1)),
            format!("winst_{}", crockford(session_seed + 2)),
            DIGEST,
            Some(format!("ps_{}", crockford(session_seed + 3))),
            Some(format!("run_{}", crockford(session_seed + 4))),
            unit_instant("2026-01-01T01:00:00.000Z"),
        )
        .expect("issuance")
    }

    fn settlement(
        grant_seed: u64,
        lease_id: &str,
        token: u64,
        session_seed: u64,
        accepted: bool,
        reason: Option<String>,
    ) -> LaunchAckSettlement {
        LaunchAckSettlement::try_new(
            grant_id(grant_seed),
            lease_id,
            token,
            session_id(session_seed),
            format!("wkr_{}", crockford(session_seed + 1)),
            format!("winst_{}", crockford(session_seed + 2)),
            accepted,
            reason,
        )
        .expect("settlement")
    }

    /// Seeds the full happy-path fixture: an online node with an active use
    /// grant, an occupied lease for the holder, and one visible binding.
    fn seed_happy_path(
        storage: &mut SqliteStorage,
        seed: u64,
        holder: &str,
    ) -> (String, String, String, u64) {
        let node = seed_online_node(storage, seed);
        stage_client_grant(storage, &node, holder, seed + 2);
        let (lease_id, token) = stage_occupied_lease(storage, &node, holder, seed + 3);
        let binding = stage_visible_binding(storage, &node, holder, seed + 6);
        (node, lease_id, binding, token)
    }

    /// Builds an issuance command from a fixed valid identity set, with the
    /// grant id, fencing token, and credential digest parameterized for the
    /// rejection cases.
    fn issuance_command(
        grant: &str,
        fencing_token: u64,
        credential_digest: &str,
    ) -> Result<LaunchGrantIssuance, WorkerLaunchGrantStoreError> {
        LaunchGrantIssuance::try_new(
            grant,
            format!("cnd_{}", crockford(1)),
            format!("cix_{}", crockford(2)),
            user_id(3),
            format!("ocl_{}", crockford(4)),
            fencing_token,
            format!("rbd_{}", crockford(5)),
            session_id(6),
            format!("wkr_{}", crockford(7)),
            format!("winst_{}", crockford(8)),
            credential_digest,
            None,
            None,
            unit_instant("2026-01-01T01:00:00.000Z"),
        )
    }

    fn settlement_command(
        accepted: bool,
        rejection_reason: Option<String>,
    ) -> Result<LaunchAckSettlement, WorkerLaunchGrantStoreError> {
        LaunchAckSettlement::try_new(
            grant_id(1),
            format!("ocl_{}", crockford(4)),
            1,
            session_id(6),
            format!("wkr_{}", crockford(7)),
            format!("winst_{}", crockford(8)),
            accepted,
            rejection_reason,
        )
    }

    #[test]
    fn issuance_validates_canonical_identities_and_bounds() {
        assert!(issuance_command(&grant_id(1), 1, DIGEST).is_ok());
        let good = issuance(
            1,
            &format!("cnd_{}", crockford(1)),
            &format!("cix_{}", crockford(2)),
            &user_id(3),
            &format!("ocl_{}", crockford(4)),
            1,
            &format!("rbd_{}", crockford(5)),
            10,
        );
        assert_eq!(good.worker_launch_grant_id(), grant_id(1));
        assert_eq!(good.worker_session_id(), session_id(10));
        let bad_ids = [
            grant_id(2).replace("wlg_", "nope_"),
            format!("cnd_{}", crockford(3)).to_uppercase(),
        ];
        for bad in bad_ids {
            assert!(
                issuance_command(&bad, 1, DIGEST).is_err(),
                "{bad} must be refused"
            );
        }
        assert!(issuance_command(&grant_id(1), 0, DIGEST).is_err());
        assert!(issuance_command(&grant_id(1), 1, "plain").is_err());
    }

    #[test]
    fn settlement_validates_reason_presence_and_shapes() {
        assert!(settlement_command(true, None).is_ok());
        assert!(settlement_command(false, Some("rejected_capacity_exhausted".to_owned())).is_ok());
        assert!(settlement_command(false, None).is_err());
        assert!(settlement_command(true, Some("no".to_owned())).is_err());
        assert!(settlement_command(false, Some(String::new())).is_err());
    }

    #[test]
    fn issue_gate_accepts_the_happy_path_and_records_the_issued_audit() {
        let mut storage = SqliteStorage::open(temporary_directory("happy")).expect("storage");
        let holder = user_id(7);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        let command = issuance(
            11,
            &node,
            &format!("cix_{}", crockford(2)),
            &holder,
            &lease_id,
            token,
            &binding,
            20,
        );
        let record = {
            let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
            ledger
                .issue(&command, &unit_instant("2026-01-01T00:02:00.000Z"))
                .expect("issue")
        };
        assert_eq!(record.state, WorkerLaunchGrantState::Issued);
        assert_eq!(record.holder_user_id, holder);
        assert_eq!(record.occupancy_lease_id, lease_id);
        assert_eq!(record.occupancy_fencing_token, token);
        assert_eq!(record.credential_digest, DIGEST);
        assert_eq!(record.revision, 1);
        let ledger = storage.worker_launch_grant_ledger().expect("ledger");
        let trail = ledger
            .audit_trail(&record.worker_launch_grant_id)
            .expect("trail");
        assert_eq!(trail.len(), 1);
        assert_eq!(trail[0].action, LaunchAuditAction::Issued);
        assert_eq!(ledger.non_terminal_count_for_node(&node).expect("count"), 1);
        assert!(
            ledger
                .active_grant_for_session(&record.worker_session_id)
                .expect("session lookup")
                .is_some()
        );
    }

    #[test]
    fn issue_gate_refuses_a_lease_that_is_not_the_holders_occupied_lease() {
        let mut storage = SqliteStorage::open(temporary_directory("lease")).expect("storage");
        let holder = user_id(7);
        let other = user_id(8);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        {
            let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
            // A lease that belongs to another user.
            let error = ledger
                .issue(
                    &issuance(
                        11,
                        &node,
                        &format!("cix_{}", crockford(2)),
                        &other,
                        &lease_id,
                        token,
                        &binding,
                        20,
                    ),
                    &unit_instant("2026-01-01T00:02:00.000Z"),
                )
                .expect_err("a foreign holder must be refused");
            assert_eq!(
                error.kind(),
                WorkerLaunchGrantStoreErrorKind::NotLeaseHolder
            );
            // A fencing token that does not match the lease.
            let error = ledger
                .issue(
                    &issuance(
                        12,
                        &node,
                        &format!("cix_{}", crockford(2)),
                        &holder,
                        &lease_id,
                        token + 1,
                        &binding,
                        21,
                    ),
                    &unit_instant("2026-01-01T00:02:00.000Z"),
                )
                .expect_err("a stale token must be refused");
            assert_eq!(
                error.kind(),
                WorkerLaunchGrantStoreErrorKind::FencingTokenMismatch
            );
        }
        // A lease the holder released no longer authorizes a launch (a node
        // holds at most one active lease, so the confirmed lease itself is
        // released to exercise the unconfirmed state).
        {
            let mut occupancy = storage.client_occupancy_ledger().expect("ledger");
            occupancy
                .request_release(
                    &lease_id,
                    token,
                    0,
                    &unit_instant("2026-01-01T00:03:00.000Z"),
                )
                .expect("release");
        }
        let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
        let error = ledger
            .issue(
                &issuance(
                    13,
                    &node,
                    &format!("cix_{}", crockford(2)),
                    &holder,
                    &lease_id,
                    token,
                    &binding,
                    22,
                ),
                &unit_instant("2026-01-01T00:03:01.000Z"),
            )
            .expect_err("an unconfirmed lease must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::OccupancyNotConfirmed
        );
        // An unknown lease.
        let error = ledger
            .issue(
                &issuance(
                    14,
                    &node,
                    &format!("cix_{}", crockford(2)),
                    &holder,
                    &format!("ocl_{}", crockford(99)),
                    1,
                    &binding,
                    23,
                ),
                &unit_instant("2026-01-01T00:03:02.000Z"),
            )
            .expect_err("an unknown lease must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::UnknownOccupancyLease
        );
    }

    #[test]
    fn issue_gate_refuses_bindings_that_are_foreign_or_invisible() {
        let mut storage = SqliteStorage::open(temporary_directory("binding")).expect("storage");
        let holder = user_id(7);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        // A second node with its own occupied lease, plus two bindings the
        // second holder cannot see (no client grant, no repository grant).
        let other_holder = user_id(9);
        let other_node = seed_online_node(&mut storage, 60);
        stage_client_grant(&mut storage, &other_node, &other_holder, 62);
        let (other_lease, other_token) =
            stage_occupied_lease(&mut storage, &other_node, &other_holder, 63);
        let foreign_binding = stage_visible_binding(&mut storage, &other_node, &other_holder, 66);
        {
            let mut binding_ledger = repository_ledger(&mut storage);
            let projection = RepositoryBindingProjection::try_new(
                format!("rbd_{}", crockford(70)),
                other_node.clone(),
                "unshared",
                None,
                None,
                RepositoryDirtyState::Clean,
                RepositoryAvailability::Available,
                format!("sha256:{:064}", 70_u64),
            )
            .expect("projection");
            binding_ledger
                .upsert(
                    &projection,
                    None,
                    0,
                    &unit_instant("2026-01-01T00:00:40.000Z"),
                )
                .expect("upsert");
        }

        let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
        // The binding belongs to another client node.
        let error = ledger
            .issue(
                &issuance(
                    21,
                    &node,
                    &format!("cix_{}", crockford(2)),
                    &holder,
                    &lease_id,
                    token,
                    &foreign_binding,
                    30,
                ),
                &unit_instant("2026-01-01T00:02:00.000Z"),
            )
            .expect_err("a foreign binding must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::BindingForeignClient
        );
        // The binding has no active repository grant for the holder.
        let error = ledger
            .issue(
                &issuance(
                    22,
                    &other_node,
                    &format!("cix_{}", crockford(61)),
                    &other_holder,
                    &other_lease,
                    other_token,
                    &format!("rbd_{}", crockford(70)),
                    31,
                ),
                &unit_instant("2026-01-01T00:02:01.000Z"),
            )
            .expect_err("an invisible binding must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::BindingNotVisible
        );
        // An unknown binding.
        let error = ledger
            .issue(
                &issuance(
                    23,
                    &node,
                    &format!("cix_{}", crockford(2)),
                    &holder,
                    &lease_id,
                    token,
                    &format!("rbd_{}", crockford(99)),
                    32,
                ),
                &unit_instant("2026-01-01T00:02:02.000Z"),
            )
            .expect_err("an unknown binding must be refused");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::UnknownRepositoryBinding
        );
        let _ = (binding, token);
    }

    #[test]
    fn issue_gate_refuses_capacity_exhaustion_and_duplicate_live_sessions() {
        let mut storage = SqliteStorage::open(temporary_directory("capacity")).expect("storage");
        let holder = user_id(7);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        // Capacity zero: the durable reservation view has no free slot.
        storage
            .connection_mut()
            .expect("connection")
            .execute(
                "UPDATE client_nodes SET max_concurrent_worker_sessions = 0
                 WHERE client_node_id = ?1",
                [&node],
            )
            .expect("capacity update");
        {
            let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
            let error = ledger
                .issue(
                    &issuance(
                        31,
                        &node,
                        &format!("cix_{}", crockford(2)),
                        &holder,
                        &lease_id,
                        token,
                        &binding,
                        40,
                    ),
                    &unit_instant("2026-01-01T00:02:00.000Z"),
                )
                .expect_err("an exhausted client must be refused");
            assert_eq!(
                error.kind(),
                WorkerLaunchGrantStoreErrorKind::CapacityExhausted
            );
        }
        // Restore capacity and issue; a second live grant for the same
        // worker session must conflict.
        storage
            .connection_mut()
            .expect("connection")
            .execute(
                "UPDATE client_nodes SET max_concurrent_worker_sessions = 4
                 WHERE client_node_id = ?1",
                [&node],
            )
            .expect("capacity restore");
        let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
        let first = ledger
            .issue(
                &issuance(
                    32,
                    &node,
                    &format!("cix_{}", crockford(2)),
                    &holder,
                    &lease_id,
                    token,
                    &binding,
                    41,
                ),
                &unit_instant("2026-01-01T00:02:01.000Z"),
            )
            .expect("first issue");
        let second = LaunchGrantIssuance::try_new(
            grant_id(33),
            node.clone(),
            format!("cix_{}", crockford(2)),
            holder.clone(),
            lease_id.clone(),
            token,
            binding.clone(),
            first.worker_session_id.clone(),
            format!("wkr_{}", crockford(99)),
            format!("winst_{}", crockford(98)),
            DIGEST,
            None,
            None,
            unit_instant("2026-01-01T01:00:00.000Z"),
        )
        .expect("second issuance");
        let error = ledger
            .issue(&second, &unit_instant("2026-01-01T00:02:02.000Z"))
            .expect_err("a second live grant per session must conflict");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::LaunchGrantConflict
        );
    }

    #[test]
    fn accepted_ack_consumes_exactly_once_and_replays_are_idempotent() {
        let mut storage = SqliteStorage::open(temporary_directory("consume")).expect("storage");
        let holder = user_id(7);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        let command = issuance(
            11,
            &node,
            &format!("cix_{}", crockford(2)),
            &holder,
            &lease_id,
            token,
            &binding,
            20,
        );
        {
            let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
            ledger
                .issue(&command, &unit_instant("2026-01-01T00:02:00.000Z"))
                .expect("issue");
        }
        let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
        let outcome = ledger
            .settle_launch_ack(
                &settlement(11, &lease_id, token, 20, true, None),
                &unit_instant("2026-01-01T00:03:00.000Z"),
            )
            .expect("settle");
        let consumed = match outcome {
            LaunchAckOutcome::Consumed(record) => *record,
            other => panic!("expected consumption, got {other:?}"),
        };
        assert_eq!(consumed.state, WorkerLaunchGrantState::Consumed);
        assert!(consumed.consumed_at.is_some());
        assert_eq!(consumed.revision, 2);
        // A replay is an accepted idempotent no-op that changes nothing.
        let outcome = ledger
            .settle_launch_ack(
                &settlement(11, &lease_id, token, 20, true, None),
                &unit_instant("2026-01-01T00:03:01.000Z"),
            )
            .expect("replay settle");
        assert_eq!(outcome, LaunchAckOutcome::AlreadyConsumed);
        let replayed = ledger
            .snapshot(&consumed.worker_launch_grant_id)
            .expect("snapshot")
            .expect("grant");
        assert_eq!(replayed.revision, 2, "the replay never rewrote the row");
        // Exactly two audit rows: issued plus consumed.
        let trail = ledger
            .audit_trail(&consumed.worker_launch_grant_id)
            .expect("trail");
        let actions = trail.iter().map(|entry| entry.action).collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![LaunchAuditAction::Issued, LaunchAuditAction::Consumed]
        );
    }

    #[test]
    fn rejected_ack_keeps_the_grant_issued_and_audits_the_reason() {
        let mut storage = SqliteStorage::open(temporary_directory("reject")).expect("storage");
        let holder = user_id(7);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        {
            let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
            ledger
                .issue(
                    &issuance(
                        11,
                        &node,
                        &format!("cix_{}", crockford(2)),
                        &holder,
                        &lease_id,
                        token,
                        &binding,
                        20,
                    ),
                    &unit_instant("2026-01-01T00:02:00.000Z"),
                )
                .expect("issue");
        }
        let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
        let outcome = ledger
            .settle_launch_ack(
                &settlement(
                    11,
                    &lease_id,
                    token,
                    20,
                    false,
                    Some("rejected_capacity_exhausted".to_owned()),
                ),
                &unit_instant("2026-01-01T00:03:00.000Z"),
            )
            .expect("settle rejection");
        let kept = match outcome {
            LaunchAckOutcome::KeptIssued(record) => *record,
            other => panic!("expected kept issued, got {other:?}"),
        };
        assert_eq!(kept.state, WorkerLaunchGrantState::Issued);
        assert_eq!(kept.revision, 1, "a rejection never rewrites the grant");
        let trail = ledger
            .audit_trail(&kept.worker_launch_grant_id)
            .expect("trail");
        assert_eq!(trail.len(), 2);
        assert_eq!(trail[1].action, LaunchAuditAction::LaunchRejected);
        assert_eq!(
            trail[1].reason.as_deref(),
            Some("rejected_capacity_exhausted")
        );
        // After the rejection the grant can still be consumed by a later
        // accepted acknowledgement.
        let outcome = ledger
            .settle_launch_ack(
                &settlement(11, &lease_id, token, 20, true, None),
                &unit_instant("2026-01-01T00:03:02.000Z"),
            )
            .expect("late accept");
        assert!(matches!(outcome, LaunchAckOutcome::Consumed(_)));
    }

    #[test]
    fn settlement_field_mismatch_and_expiry_refuse_consumption() {
        let mut storage = SqliteStorage::open(temporary_directory("mismatch")).expect("storage");
        let holder = user_id(7);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        {
            let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
            ledger
                .issue(
                    &issuance(
                        11,
                        &node,
                        &format!("cix_{}", crockford(2)),
                        &holder,
                        &lease_id,
                        token,
                        &binding,
                        20,
                    ),
                    &unit_instant("2026-01-01T00:02:00.000Z"),
                )
                .expect("issue");
        }
        let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
        // A mismatched worker instance refuses with no state change.
        let mismatched = LaunchAckSettlement::try_new(
            grant_id(11),
            lease_id.clone(),
            token,
            session_id(20),
            format!("wkr_{}", crockford(21)),
            format!("winst_{}", crockford(99)),
            true,
            None,
        )
        .expect("mismatched settlement");
        let error = ledger
            .settle_launch_ack(&mismatched, &unit_instant("2026-01-01T00:03:00.000Z"))
            .expect_err("a field mismatch must refuse");
        assert_eq!(error.kind(), WorkerLaunchGrantStoreErrorKind::FieldMismatch);
        // A stale fencing token refuses with no state change.
        let error = ledger
            .settle_launch_ack(
                &settlement(11, &lease_id, token + 1, 20, true, None),
                &unit_instant("2026-01-01T00:03:01.000Z"),
            )
            .expect_err("a stale token must refuse");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::FencingTokenMismatch
        );
        // An acknowledgement after the expiry deadline refuses.
        let error = ledger
            .settle_launch_ack(
                &settlement(11, &lease_id, token, 20, true, None),
                &unit_instant("2026-01-01T02:00:00.000Z"),
            )
            .expect_err("an expired grant must refuse consumption");
        assert_eq!(error.kind(), WorkerLaunchGrantStoreErrorKind::GrantExpired);
        let still_issued = ledger
            .snapshot(&grant_id(11))
            .expect("snapshot")
            .expect("grant");
        assert_eq!(still_issued.state, WorkerLaunchGrantState::Issued);
        assert_eq!(still_issued.revision, 1);
    }

    #[test]
    fn revoke_terminates_an_issued_grant_and_consumed_grants_are_irrevocable() {
        let mut storage = SqliteStorage::open(temporary_directory("terminal")).expect("storage");
        let holder = user_id(7);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        {
            let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
            ledger
                .issue(
                    &issuance(
                        11,
                        &node,
                        &format!("cix_{}", crockford(2)),
                        &holder,
                        &lease_id,
                        token,
                        &binding,
                        20,
                    ),
                    &unit_instant("2026-01-01T00:02:00.000Z"),
                )
                .expect("issue");
            ledger
                .issue(
                    &issuance(
                        12,
                        &node,
                        &format!("cix_{}", crockford(2)),
                        &holder,
                        &lease_id,
                        token,
                        &binding,
                        30,
                    ),
                    &unit_instant("2026-01-01T00:02:01.000Z"),
                )
                .expect("second issue");
        }
        let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
        let revoked = ledger
            .revoke(
                &grant_id(11),
                &holder,
                Some("superseded by a fresh launch"),
                &unit_instant("2026-01-01T00:04:00.000Z"),
            )
            .expect("revoke");
        assert_eq!(revoked.state, WorkerLaunchGrantState::Revoked);
        assert!(revoked.ended_at.is_some());
        // A consumed grant cannot be revoked.
        ledger
            .settle_launch_ack(
                &settlement(12, &lease_id, token, 30, true, None),
                &unit_instant("2026-01-01T00:04:01.000Z"),
            )
            .expect("consume second");
        let error = ledger
            .revoke(
                &grant_id(12),
                &holder,
                None,
                &unit_instant("2026-01-01T00:04:02.000Z"),
            )
            .expect_err("a consumed grant cannot be revoked");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::IllegalStateTransition
        );
        // The expiry sweep terminates every issued grant past the cutoff.
        let expired = ledger
            .expire(&unit_instant("2026-01-01T02:00:00.000Z"))
            .expect("expire");
        assert!(expired.is_empty(), "no issued grants remain");
        let trail = ledger
            .audit_trail(&revoked.worker_launch_grant_id)
            .expect("trail");
        let actions = trail.iter().map(|entry| entry.action).collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![LaunchAuditAction::Issued, LaunchAuditAction::Revoked]
        );
        assert_eq!(trail[1].actor_user_id, holder);
        assert_eq!(
            trail[1].reason.as_deref(),
            Some("superseded by a fresh launch")
        );
    }

    #[test]
    fn expiry_sweep_terminates_overdue_issued_grants() {
        let mut storage = SqliteStorage::open(temporary_directory("expire")).expect("storage");
        let holder = user_id(7);
        let (node, lease_id, binding, token) = seed_happy_path(&mut storage, 1, &holder);
        {
            let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
            ledger
                .issue(
                    &issuance(
                        11,
                        &node,
                        &format!("cix_{}", crockford(2)),
                        &holder,
                        &lease_id,
                        token,
                        &binding,
                        20,
                    ),
                    &unit_instant("2026-01-01T00:02:00.000Z"),
                )
                .expect("issue");
        }
        let mut ledger = storage.worker_launch_grant_ledger().expect("ledger");
        let expired = ledger
            .expire(&unit_instant("2026-01-01T02:00:00.000Z"))
            .expect("expire");
        assert_eq!(expired, vec![grant_id(11)]);
        let record = ledger
            .snapshot(&grant_id(11))
            .expect("snapshot")
            .expect("grant");
        assert_eq!(record.state, WorkerLaunchGrantState::Expired);
        assert!(record.ended_at.is_some());
        // An expired grant is terminal: no acknowledgement changes it.
        let error = ledger
            .settle_launch_ack(
                &settlement(11, &lease_id, token, 20, true, None),
                &unit_instant("2026-01-01T03:00:00.000Z"),
            )
            .expect_err("an expired grant is terminal");
        assert_eq!(
            error.kind(),
            WorkerLaunchGrantStoreErrorKind::IllegalStateTransition
        );
        let trail = ledger.audit_trail(&grant_id(11)).expect("trail");
        assert_eq!(trail.len(), 2);
        assert_eq!(trail[1].action, LaunchAuditAction::Expired);
    }

    #[test]
    fn audit_identities_derive_deterministically_per_action() {
        let grant = grant_id(11);
        assert_eq!(
            audit_identity(LaunchAuditAction::Issued, &grant).expect("identity"),
            format!("wla_01{}", &grant[4..28])
        );
        assert_ne!(
            audit_identity(LaunchAuditAction::Issued, &grant).expect("identity"),
            audit_identity(LaunchAuditAction::Consumed, &grant).expect("identity")
        );
        assert!(
            validate_crockford_id(
                &audit_identity(LaunchAuditAction::LaunchRejected, &grant).expect("identity"),
                "wla_",
                "launch audit id"
            )
            .is_ok()
        );
    }
}
