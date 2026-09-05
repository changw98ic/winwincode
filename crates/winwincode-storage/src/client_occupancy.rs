// SPDX-License-Identifier: Apache-2.0

//! Durable Server-side `ClientOccupancyLease` registry and the global
//! occupancy fencing-token allocator.
//!
//! The Control Plane is the authoritative owner of client occupancy (ADR-0030,
//! plan 6): it atomically creates one occupancy lease per claim, mints a
//! strictly higher fencing token for every new occupancy, and only the Device
//! Client ACK of the exact lease and token promotes a lease from `reserving`
//! to `occupied` (plan 12.2, 12.6). States and legal transitions follow the
//! frozen state machine in `docs/contracts/client-control-state-machines.md`
//! contract 4: `available` is the projection of "no active lease", the active
//! lease starts at `reserving`, at most one active lease exists per
//! `clientNodeId` (enforced durably by a partial unique index), a `reserving`
//! lease without an ACK terminates as `released` with the `release_reason`
//! naming why, `recovery_pending` has no automatic terminal state, an idle
//! `occupied` lease expires, and `draining` always ends in an automatic
//! `released`.
//!
//! Capacity note: the claim gate requires at least one free worker-session
//! slot using the device-reported running count plus the slot the occupancy is
//! about to consume (`reportedRunning + reserved < max`, with `reserved`
//! simplified to zero durable slots at claim time). The durable per-slot
//! reservation ledger belongs to the execution FLOW epic and will tighten this
//! check when it lands.

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;

const CLIENT_OCCUPANCY_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS client_occupancy_leases (
    occupancy_lease_id TEXT PRIMARY KEY NOT NULL,
    client_node_id TEXT NOT NULL,
    holder_user_id TEXT NOT NULL,
    fencing_token INTEGER NOT NULL UNIQUE
        CHECK (fencing_token > 0 AND fencing_token <= 9007199254740991),
    claim_request_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN (
        'reserving', 'occupied', 'draining', 'recovery_pending', 'released', 'expired')),
    acknowledged_at TEXT,
    claimed_at TEXT,
    last_renewed_at TEXT,
    idle_expires_at TEXT,
    recovery_deadline_at TEXT,
    release_reason TEXT CHECK (release_reason IN (
        'ack_timeout', 'client_rejected', 'claim_withdrawn',
        'holder_released', 'drain_completed', 'force_released')),
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX IF NOT EXISTS client_occupancy_leases_one_active_per_client
    ON client_occupancy_leases (client_node_id)
    WHERE state IN ('reserving', 'occupied', 'draining', 'recovery_pending');
CREATE INDEX IF NOT EXISTS client_occupancy_leases_by_client
    ON client_occupancy_leases (client_node_id, state);
CREATE TABLE IF NOT EXISTS occupancy_fencing_tokens (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    last_issued_token INTEGER NOT NULL
        CHECK (last_issued_token >= 0 AND last_issued_token <= 9007199254740991)
);
";

/// Lifecycle state of one `ClientOccupancyLease` (contract 4).
///
/// `available` is deliberately absent: it is the projection of "this client
/// node has no active lease", never a stored state. `released` and `expired`
/// are the terminal states kept as audit history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OccupancyLeaseState {
    /// Offer created and waiting for the Device Client ACK of its token.
    Reserving,
    /// The Device Client acknowledged the exact lease and fencing token.
    Occupied,
    /// Release requested while worker sessions are still active.
    Draining,
    /// The client dropped while occupied or draining; reconciliation pending.
    RecoveryPending,
    /// Terminal: released explicitly, automatically, or by safe cleanup.
    Released,
    /// Terminal: an idle occupied lease reached its idle policy deadline.
    Expired,
}

impl OccupancyLeaseState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserving => "reserving",
            Self::Occupied => "occupied",
            Self::Draining => "draining",
            Self::RecoveryPending => "recovery_pending",
            Self::Released => "released",
            Self::Expired => "expired",
        }
    }

    fn parse(value: &str) -> Result<Self, OccupancyStoreError> {
        match value {
            "reserving" => Ok(Self::Reserving),
            "occupied" => Ok(Self::Occupied),
            "draining" => Ok(Self::Draining),
            "recovery_pending" => Ok(Self::RecoveryPending),
            "released" => Ok(Self::Released),
            "expired" => Ok(Self::Expired),
            _ => Err(error(
                OccupancyStoreErrorKind::CorruptState,
                "stored occupancy lease state is invalid",
            )),
        }
    }

    /// True while the lease still occupies the client node's single active
    /// slot, so a new claim must be rejected.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Reserving | Self::Occupied | Self::Draining | Self::RecoveryPending
        )
    }
}

impl fmt::Display for OccupancyLeaseState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Durable reason distinguishing why a lease ended (contract 4, contract 10
/// open point 2): the `reserving` release names whether the ACK window
/// elapsed, the Device Client rejected the offer, or the applicant withdrew.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OccupancyReleaseReason {
    /// `reserving -> released`: the offer was not `ACKed` within its window.
    AckTimeout,
    /// `reserving -> released`: the Device Client reported
    /// `client.occupancy.rejected`.
    ClientRejected,
    /// `reserving -> released`: the applicant withdrew the claim.
    ClaimWithdrawn,
    /// `occupied -> released`: the holder released with no active task.
    HolderReleased,
    /// `draining -> released`: every worker session reached a terminal state.
    DrainCompleted,
    /// `recovery_pending -> released`: administrator or original holder
    /// executed the safe cleanup after the recovery deadline passed.
    ForceReleased,
}

impl OccupancyReleaseReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AckTimeout => "ack_timeout",
            Self::ClientRejected => "client_rejected",
            Self::ClaimWithdrawn => "claim_withdrawn",
            Self::HolderReleased => "holder_released",
            Self::DrainCompleted => "drain_completed",
            Self::ForceReleased => "force_released",
        }
    }

    fn parse(value: &str) -> Result<Self, OccupancyStoreError> {
        match value {
            "ack_timeout" => Ok(Self::AckTimeout),
            "client_rejected" => Ok(Self::ClientRejected),
            "claim_withdrawn" => Ok(Self::ClaimWithdrawn),
            "holder_released" => Ok(Self::HolderReleased),
            "drain_completed" => Ok(Self::DrainCompleted),
            "force_released" => Ok(Self::ForceReleased),
            _ => Err(error(
                OccupancyStoreErrorKind::CorruptState,
                "stored occupancy release reason is invalid",
            )),
        }
    }
}

impl fmt::Display for OccupancyReleaseReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Recovery reconciliation target of a `recovery_pending` lease (contract 4).
///
/// The fencing token is never changed by a resume: no new occupancy happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OccupancyReconcileTarget {
    /// The reconcile report says local workers are still running.
    ResumeOccupied,
    /// The reconcile report says tasks ended or are wrapping up.
    ResumeDraining,
}

/// Validated atomic claim command (plan 12.2).
///
/// The `_id` postfix on every field is the plan's own domain vocabulary, so
/// the lint against repeated field suffixes is intentionally allowed here.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OccupancyClaim {
    occupancy_lease_id: String,
    client_node_id: String,
    holder_user_id: String,
    claim_request_id: String,
}

impl OccupancyClaim {
    /// Builds one validated claim command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical lease, client node, user, and claim request
    /// identities before any durable write.
    pub fn try_new(
        occupancy_lease_id: impl Into<String>,
        client_node_id: impl Into<String>,
        holder_user_id: impl Into<String>,
        claim_request_id: impl Into<String>,
    ) -> Result<Self, OccupancyStoreError> {
        let claim = Self {
            occupancy_lease_id: occupancy_lease_id.into(),
            client_node_id: client_node_id.into(),
            holder_user_id: holder_user_id.into(),
            claim_request_id: claim_request_id.into(),
        };
        validate_occupancy_lease_id(&claim.occupancy_lease_id)?;
        validate_client_node_id(&claim.client_node_id)?;
        validate_user_id(&claim.holder_user_id)?;
        validate_claim_request_id(&claim.claim_request_id)?;
        Ok(claim)
    }

    #[must_use]
    pub fn occupancy_lease_id(&self) -> &str {
        &self.occupancy_lease_id
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
    pub fn claim_request_id(&self) -> &str {
        &self.claim_request_id
    }
}

/// Durable `ClientOccupancyLease` row (plan 7.5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OccupancyLeaseRecord {
    /// Stable occupancy lease identifier.
    pub occupancy_lease_id: String,
    /// Occupied client node.
    pub client_node_id: String,
    /// Occupancy belongs to this user, never to a browser cookie.
    pub holder_user_id: String,
    /// Monotonic fencing token minted for exactly this occupancy.
    pub fencing_token: u64,
    /// Caller-supplied claim request identity.
    pub claim_request_id: String,
    /// Lifecycle state.
    pub state: OccupancyLeaseState,
    /// Device Client ACK instant; set exactly on `reserving -> occupied`.
    pub acknowledged_at: Option<Instant>,
    /// Instant the claim created this lease in `reserving`.
    pub claimed_at: Option<Instant>,
    /// Last renewal instant (ACK and reconciliation resumes).
    pub last_renewed_at: Option<Instant>,
    /// Idle policy deadline owned by the caller; only `occupied` uses it.
    pub idle_expires_at: Option<Instant>,
    /// Recovery window deadline; only `recovery_pending` uses it.
    pub recovery_deadline_at: Option<Instant>,
    /// Terminal discriminator; set on every `released` transition.
    pub release_reason: Option<OccupancyReleaseReason>,
    /// Instant the lease record was created.
    pub created_at: Instant,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Stable occupancy ledger failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OccupancyStoreErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// No occupancy lease matches the requested identity.
    UnknownOccupancyLease,
    /// The holder has no active `use` access grant on the client node.
    AccessDenied,
    /// The client node presence is not `online`.
    PresenceNotOnline,
    /// The client node is `locked`.
    ClientLocked,
    /// The client node does not accept new occupancy.
    NotAcceptingConnections,
    /// The client node has no free worker-session slot.
    CapacityExhausted,
    /// An active lease already occupies the client node.
    ActiveLeaseConflict,
    /// The occupancy lease id is already used.
    OccupancyLeaseConflict,
    /// The command carried a fencing token other than the lease's token.
    FencingTokenMismatch,
    /// The durable fencing-token counter reached the safe integer ceiling.
    FencingTokenExhausted,
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

/// Secret-free occupancy ledger error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OccupancyStoreError {
    kind: OccupancyStoreErrorKind,
    message: String,
}

impl OccupancyStoreError {
    #[must_use]
    pub const fn kind(&self) -> OccupancyStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for OccupancyStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OccupancyStoreError {}

/// Occupancy lease ledger and fencing-token allocator borrowing the sole
/// product-state `SQLite` authority.
pub struct OccupancyLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable occupancy lease ledger on this same product-state
    /// database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
    pub fn client_occupancy_ledger(&mut self) -> Result<OccupancyLedger<'_>, OccupancyStoreError> {
        OccupancyLedger::new(self)
    }
}

impl<'storage> OccupancyLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, OccupancyStoreError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(CLIENT_OCCUPANCY_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Returns the highest fencing token issued so far; zero before the first
    /// occupancy.
    ///
    /// # Errors
    ///
    /// Rejects a corrupt counter row or storage failure.
    pub fn current_fencing_token(&self) -> Result<u64, OccupancyStoreError> {
        let connection = self.connection()?;
        let stored: Option<i64> = connection
            .query_row(
                "SELECT last_issued_token FROM occupancy_fencing_tokens WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|sql| sql_error(&sql))?;
        stored
            .map(|value| from_sql_integer(value, "fencing token"))
            .transpose()
            .map(|value| value.unwrap_or(0))
    }

    /// Mints the next global fencing token; strictly higher than every token
    /// minted before on this database.
    ///
    /// Only new occupancies mint tokens. Recovery and reconciliation resume a
    /// lease under its original token and must not call this.
    ///
    /// # Errors
    ///
    /// Rejects a counter at the safe integer ceiling or storage failure.
    pub fn mint_fencing_token(&mut self) -> Result<u64, OccupancyStoreError> {
        let transaction = self.transaction()?;
        let token = mint_fencing_token(&transaction)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(token)
    }

    /// Atomically claims occupancy of one client node (plan 12.2).
    ///
    /// Inside one immediate transaction the five-condition gate reuses the
    /// durable registry and connect-ledger facts: the holder has an active
    /// `use` grant, presence is `online` and the node is not `locked`, the
    /// node accepts new occupancy, no active lease exists (the partial unique
    /// index is the durable backstop), and at least one worker-session slot is
    /// free (`reportedRunning + reserved < max`, with the durable reservation
    /// ledger deferred to the FLOW epic). On success a `reserving` lease is
    /// created with a freshly minted fencing token. Concurrent claims are
    /// serialized by the immediate transaction: exactly one wins.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, a missing or `use`-less grant, a
    /// non-`online` or `locked` node, a node that is not accepting, an
    /// exhausted capacity, an already active lease, a reused lease id, or
    /// storage failure.
    pub fn atomic_claim(
        &mut self,
        claim: &OccupancyClaim,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
        validate_instant(now, "claim time")?;
        let transaction = self.transaction()?;
        ensure_claim_gate(&transaction, claim)?;
        let fencing_token = mint_fencing_token(&transaction)?;
        let inserted = transaction
            .execute(
                "INSERT INTO client_occupancy_leases
                 (occupancy_lease_id, client_node_id, holder_user_id, fencing_token,
                  claim_request_id, state, acknowledged_at, claimed_at, last_renewed_at,
                  idle_expires_at, recovery_deadline_at, release_reason, created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'reserving', NULL, ?6, NULL, NULL, NULL,
                         NULL, ?6, 1)",
                params![
                    claim.occupancy_lease_id(),
                    claim.client_node_id(),
                    claim.holder_user_id(),
                    sql_integer(fencing_token)?,
                    claim.claim_request_id(),
                    now.0,
                ],
            )
            .map_err(|sql| map_lease_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                OccupancyStoreErrorKind::Storage,
                "occupancy lease insert did not store exactly one row",
            ));
        }
        let record = load_occupancy_lease(&transaction, claim.occupancy_lease_id())?
            .ok_or_else(lease_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Records the Device Client ACK (`client.occupancy.ack`, plan 12.2) and
    /// promotes `reserving -> occupied` only when both the lease id and the
    /// fencing token match.
    ///
    /// The caller owns the idle policy through `idle_expires_at`; only an
    /// `occupied` lease uses it. An ACK replay for an already `occupied` lease
    /// carrying the matching token is an accepted idempotent no-op.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a token mismatch, a non-`reserving` lease,
    /// or storage failure.
    pub fn record_acknowledgement(
        &mut self,
        occupancy_lease_id: &str,
        fencing_token: u64,
        idle_expires_at: Option<&Instant>,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
        validate_occupancy_lease_id(occupancy_lease_id)?;
        validate_fencing_token(fencing_token)?;
        if let Some(idle) = idle_expires_at {
            validate_instant(idle, "idle expiry")?;
        }
        validate_instant(now, "acknowledgement time")?;
        let transaction = self.transaction()?;
        let record = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        if record.state == OccupancyLeaseState::Occupied {
            ensure_fencing_token(&record, fencing_token)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state != OccupancyLeaseState::Reserving {
            return Err(illegal_transition(&record, "acknowledgement"));
        }
        ensure_fencing_token(&record, fencing_token)?;
        let updated = transaction
            .execute(
                "UPDATE client_occupancy_leases
                 SET state = 'occupied', acknowledged_at = ?2, last_renewed_at = ?2,
                     idle_expires_at = ?3, revision = revision + 1
                 WHERE occupancy_lease_id = ?1
                   AND state = 'reserving' AND fencing_token = ?4",
                params![
                    occupancy_lease_id,
                    now.0,
                    idle_expires_at.map(|instant| instant.0.clone()),
                    sql_integer(fencing_token)?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("acknowledgement"));
        }
        let updated = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Terminates a `reserving` lease as `released` (contract 4): the offer
    /// was not `ACKed` within its window, the Device Client reported
    /// `client.occupancy.rejected`, or the applicant withdrew.
    ///
    /// The `release_reason` distinguishes the three paths. A replay with the
    /// same reason is an accepted idempotent no-op.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a token mismatch, a reason that does not
    /// belong to the `reserving` terminal paths, a non-`reserving` lease, or
    /// storage failure.
    pub fn reject_offer(
        &mut self,
        occupancy_lease_id: &str,
        fencing_token: u64,
        reason: OccupancyReleaseReason,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
        validate_occupancy_lease_id(occupancy_lease_id)?;
        validate_fencing_token(fencing_token)?;
        validate_instant(now, "rejection time")?;
        if !matches!(
            reason,
            OccupancyReleaseReason::AckTimeout
                | OccupancyReleaseReason::ClientRejected
                | OccupancyReleaseReason::ClaimWithdrawn
        ) {
            return Err(error(
                OccupancyStoreErrorKind::InvalidInput,
                "only ack timeout, client rejection, or claim withdrawal can end a reserving lease",
            ));
        }
        let transaction = self.transaction()?;
        let record = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        if record.state == OccupancyLeaseState::Released {
            ensure_replay_reason(&record, reason)?;
            ensure_fencing_token(&record, fencing_token)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state != OccupancyLeaseState::Reserving {
            return Err(illegal_transition(&record, "offer rejection"));
        }
        ensure_fencing_token(&record, fencing_token)?;
        release_lease(
            &transaction,
            occupancy_lease_id,
            OccupancyLeaseState::Reserving,
            fencing_token,
            reason,
        )?;
        let updated = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Applies the holder's release request to an `occupied` lease (plan
    /// 12.4): no active worker session releases immediately, any active
    /// worker session moves the lease to `draining`.
    ///
    /// The active worker-session count is a skeleton input until the FLOW
    /// epic lands the durable task ledger. `draining` replays with an active
    /// count are accepted idempotent no-ops; a zero count against `draining`
    /// is refused because only [`Self::drain_complete`] may release it.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a token mismatch, a non-`occupied` lease, or
    /// storage failure.
    pub fn request_release(
        &mut self,
        occupancy_lease_id: &str,
        fencing_token: u64,
        active_worker_session_count: u64,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
        validate_occupancy_lease_id(occupancy_lease_id)?;
        validate_fencing_token(fencing_token)?;
        validate_instant(now, "release request time")?;
        let transaction = self.transaction()?;
        let record = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        ensure_fencing_token(&record, fencing_token)?;
        if record.state == OccupancyLeaseState::Released {
            ensure_replay_reason(&record, OccupancyReleaseReason::HolderReleased)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state == OccupancyLeaseState::Draining {
            if active_worker_session_count == 0 {
                return Err(illegal_transition(&record, "release request"));
            }
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state != OccupancyLeaseState::Occupied {
            return Err(illegal_transition(&record, "release request"));
        }
        if active_worker_session_count == 0 {
            release_lease(
                &transaction,
                occupancy_lease_id,
                OccupancyLeaseState::Occupied,
                fencing_token,
                OccupancyReleaseReason::HolderReleased,
            )?;
        } else {
            let updated = transaction
                .execute(
                    "UPDATE client_occupancy_leases
                     SET state = 'draining', revision = revision + 1
                     WHERE occupancy_lease_id = ?1
                       AND state = 'occupied' AND fencing_token = ?2",
                    params![occupancy_lease_id, sql_integer(fencing_token)?],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated != 1 {
                return Err(cas_lost("release request"));
            }
        }
        let updated = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Applies the automatic `draining -> released` judgement (contract 4):
    /// once every worker session reached a terminal state the lease releases
    /// with `drain_completed`. No Device Client ACK is required.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a lease that is not `draining`, or storage
    /// failure.
    pub fn drain_complete(
        &mut self,
        occupancy_lease_id: &str,
    ) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
        validate_occupancy_lease_id(occupancy_lease_id)?;
        let transaction = self.transaction()?;
        let record = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        if record.state == OccupancyLeaseState::Released {
            ensure_replay_reason(&record, OccupancyReleaseReason::DrainCompleted)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state != OccupancyLeaseState::Draining {
            return Err(illegal_transition(&record, "drain completion"));
        }
        let updated = transaction
            .execute(
                "UPDATE client_occupancy_leases
                 SET state = 'released', release_reason = 'drain_completed',
                     revision = revision + 1
                 WHERE occupancy_lease_id = ?1 AND state = 'draining'",
                params![occupancy_lease_id],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("drain completion"));
        }
        let updated = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Projects an `occupied` or `draining` lease to `recovery_pending`
    /// because the client dropped (plan 12.5, contract 4).
    ///
    /// The client node presence must already be projected `offline` by the
    /// registry. The caller owns the recovery-window policy through
    /// `recovery_deadline_at`. There is no automatic terminal state: until an
    /// accepted reconciliation or an explicit safe cleanup, no other user may
    /// preempt the occupancy. A replay against an already
    /// `recovery_pending` lease is an accepted idempotent no-op.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, an unknown client node, a lease that is
    /// neither `occupied` nor `draining`, a node whose presence is not
    /// `offline`, or storage failure.
    pub fn mark_recovery_pending(
        &mut self,
        occupancy_lease_id: &str,
        recovery_deadline_at: &Instant,
    ) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
        validate_occupancy_lease_id(occupancy_lease_id)?;
        validate_instant(recovery_deadline_at, "recovery deadline")?;
        let transaction = self.transaction()?;
        let record = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        if record.state == OccupancyLeaseState::RecoveryPending {
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if !matches!(
            record.state,
            OccupancyLeaseState::Occupied | OccupancyLeaseState::Draining
        ) {
            return Err(illegal_transition(&record, "recovery marking"));
        }
        let presence: Option<String> = transaction
            .query_row(
                "SELECT presence_state FROM client_nodes WHERE client_node_id = ?1",
                [record.client_node_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|sql| sql_error(&sql))?;
        match presence.as_deref() {
            None => return Err(unknown_client_node()),
            Some("offline") => {}
            Some(_) => {
                return Err(error(
                    OccupancyStoreErrorKind::PresenceNotOnline,
                    "occupancy recovery marking requires the client node presence to be offline",
                ));
            }
        }
        let updated = transaction
            .execute(
                "UPDATE client_occupancy_leases
                 SET state = 'recovery_pending', recovery_deadline_at = ?2,
                     revision = revision + 1
                 WHERE occupancy_lease_id = ?1 AND state IN ('occupied', 'draining')",
                params![occupancy_lease_id, recovery_deadline_at.0],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("recovery marking"));
        }
        let updated = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Applies an accepted `client.worker.reconcile` outcome (plan 12.5,
    /// contract 4): `recovery_pending -> occupied` when local workers are
    /// still running, or `recovery_pending -> draining` when tasks ended or
    /// are wrapping up.
    ///
    /// The fencing token is reused unchanged because no new occupancy
    /// happened. `idle_expires_at` refreshes the idle policy when resuming to
    /// `occupied` and must be `None` when resuming to `draining`. A replay
    /// whose target equals the current state is an accepted idempotent no-op.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a lease that is not `recovery_pending`, an
    /// idle expiry supplied for a draining resume, or storage failure.
    pub fn reconcile_resume(
        &mut self,
        occupancy_lease_id: &str,
        target: OccupancyReconcileTarget,
        idle_expires_at: Option<&Instant>,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
        validate_occupancy_lease_id(occupancy_lease_id)?;
        if let Some(idle) = idle_expires_at {
            validate_instant(idle, "idle expiry")?;
        }
        validate_instant(now, "reconciliation time")?;
        if target == OccupancyReconcileTarget::ResumeDraining && idle_expires_at.is_some() {
            return Err(error(
                OccupancyStoreErrorKind::InvalidInput,
                "draining leases do not carry an idle expiry",
            ));
        }
        let transaction = self.transaction()?;
        let record = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        let target_state = match target {
            OccupancyReconcileTarget::ResumeOccupied => OccupancyLeaseState::Occupied,
            OccupancyReconcileTarget::ResumeDraining => OccupancyLeaseState::Draining,
        };
        if record.state == target_state {
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state != OccupancyLeaseState::RecoveryPending {
            return Err(illegal_transition(&record, "reconciliation resume"));
        }
        let updated = transaction
            .execute(
                "UPDATE client_occupancy_leases
                 SET state = ?2, last_renewed_at = ?3,
                     idle_expires_at = ?4, recovery_deadline_at = NULL,
                     revision = revision + 1
                 WHERE occupancy_lease_id = ?1 AND state = 'recovery_pending'",
                params![
                    occupancy_lease_id,
                    target_state.as_str(),
                    now.0,
                    idle_expires_at.map(|instant| instant.0.clone()),
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("reconciliation resume"));
        }
        let updated = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Releases a `recovery_pending` lease whose recovery window has passed
    /// through the explicit administrator or original-holder safe cleanup
    /// (plan 12.5, contract 4).
    ///
    /// The occupancy is never handed to a new user automatically: only this
    /// explicit cleanup returns the client node to `available`.
    ///
    /// # Errors
    ///
    /// Rejects an unknown lease, a lease that is not `recovery_pending`, a
    /// cleanup attempted before the recovery deadline, a corrupt deadline, or
    /// storage failure.
    pub fn force_release(
        &mut self,
        occupancy_lease_id: &str,
        now: &Instant,
    ) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
        validate_occupancy_lease_id(occupancy_lease_id)?;
        validate_instant(now, "force release time")?;
        let transaction = self.transaction()?;
        let record = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        if record.state == OccupancyLeaseState::Released {
            ensure_replay_reason(&record, OccupancyReleaseReason::ForceReleased)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if record.state != OccupancyLeaseState::RecoveryPending {
            return Err(illegal_transition(&record, "force release"));
        }
        let deadline = record.recovery_deadline_at.as_ref().ok_or_else(|| {
            error(
                OccupancyStoreErrorKind::CorruptState,
                "recovery pending lease is missing its recovery deadline",
            )
        })?;
        if now.0.as_str() < deadline.0.as_str() {
            return Err(error(
                OccupancyStoreErrorKind::IllegalStateTransition,
                "the recovery window is still open; safe cleanup must wait for the deadline",
            ));
        }
        release_lease(
            &transaction,
            occupancy_lease_id,
            OccupancyLeaseState::RecoveryPending,
            record.fencing_token,
            OccupancyReleaseReason::ForceReleased,
        )?;
        let updated = require_occupancy_lease(&transaction, occupancy_lease_id)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated)
    }

    /// Expires idle `occupied` leases whose `idle_expires_at` deadline is at
    /// or before `cutoff` and that still have no active worker session (plan
    /// 12.4 idle policy, contract 4).
    ///
    /// The active worker-session count is supplied per client node by the
    /// caller as a skeleton input until the FLOW epic lands the durable task
    /// ledger; leases with an active count are skipped, never expired.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire_idle(
        &mut self,
        cutoff: &Instant,
        active_worker_session_count: impl Fn(&str) -> u64,
    ) -> Result<Vec<String>, OccupancyStoreError> {
        validate_instant(cutoff, "idle expiry cutoff")?;
        let transaction = self.transaction()?;
        let mut statement = transaction
            .prepare(
                "SELECT occupancy_lease_id, client_node_id FROM client_occupancy_leases
                 WHERE state = 'occupied'
                   AND idle_expires_at IS NOT NULL AND idle_expires_at <= ?1",
            )
            .map_err(|sql| sql_error(&sql))?;
        let candidates = statement
            .query_map([cutoff.0.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        let mut expired = Vec::with_capacity(candidates.len());
        for (occupancy_lease_id, client_node_id) in candidates {
            if active_worker_session_count(&client_node_id) > 0 {
                continue;
            }
            let updated = transaction
                .execute(
                    "UPDATE client_occupancy_leases
                     SET state = 'expired', revision = revision + 1
                     WHERE occupancy_lease_id = ?1 AND state = 'occupied'",
                    params![occupancy_lease_id],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated == 1 {
                expired.push(occupancy_lease_id);
            }
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(expired)
    }

    /// Returns one durable occupancy lease projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical lease identity, corrupt stored rows, or
    /// storage failure.
    pub fn snapshot(
        &self,
        occupancy_lease_id: &str,
    ) -> Result<Option<OccupancyLeaseRecord>, OccupancyStoreError> {
        validate_occupancy_lease_id(occupancy_lease_id)?;
        load_occupancy_lease(self.connection()?, occupancy_lease_id)
    }

    /// Returns the one active lease of a client node, if any.
    ///
    /// `None` means the occupancy projection is `available`. More than one
    /// active row is a corrupt database and fails closed.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity, a corrupt active-lease
    /// set, or storage failure.
    pub fn active_lease_for_node(
        &self,
        client_node_id: &str,
    ) -> Result<Option<OccupancyLeaseRecord>, OccupancyStoreError> {
        validate_client_node_id(client_node_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT occupancy_lease_id FROM client_occupancy_leases
                 WHERE client_node_id = ?1
                   AND state IN ('reserving', 'occupied', 'draining', 'recovery_pending')",
            )
            .map_err(|sql| sql_error(&sql))?;
        let ids = statement
            .query_map([client_node_id], |row| row.get::<_, String>(0))
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        match ids.len() {
            0 => Ok(None),
            1 => load_occupancy_lease(connection, &ids[0]),
            _ => Err(error(
                OccupancyStoreErrorKind::CorruptState,
                "client node holds more than one active occupancy lease",
            )),
        }
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, OccupancyStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&rusqlite::Connection, OccupancyStoreError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

/// Applies the five-condition claim gate against the durable registry and
/// connect-ledger facts inside the caller's transaction (plan 12.2).
///
/// The client node is judged first so an unknown identity reports precisely
/// instead of masking as a missing grant; the remaining conditions follow the
/// plan order (grant, no active lease, capacity).
fn ensure_claim_gate(
    transaction: &Transaction<'_>,
    claim: &OccupancyClaim,
) -> Result<(), OccupancyStoreError> {
    // The client node must exist, be online, unlocked, and accepting.
    let node = transaction
        .query_row(
            "SELECT presence_state, lock_state, accepting_connections,
                    max_concurrent_worker_sessions, reported_running_worker_sessions
             FROM client_nodes WHERE client_node_id = ?1",
            [claim.client_node_id()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    let Some((presence, lock_state, accepting, max_slots, running)) = node else {
        return Err(unknown_client_node());
    };
    if lock_state == "locked" {
        return Err(error(
            OccupancyStoreErrorKind::ClientLocked,
            "client node is locked",
        ));
    }
    if presence != "online" {
        return Err(error(
            OccupancyStoreErrorKind::PresenceNotOnline,
            "client node presence is not online",
        ));
    }
    if accepting != 1 {
        return Err(error(
            OccupancyStoreErrorKind::NotAcceptingConnections,
            "client node does not accept new occupancy",
        ));
    }
    // Capacity: at least one free worker-session slot. The durable
    // per-slot reservation ledger belongs to the FLOW epic; until then
    // `reserved` is zero at claim time, so the check is
    // `reportedRunning + 1 <= max`.
    let max_slots = from_sql_integer(max_slots, "client worker session capacity")?;
    let running = from_sql_integer(running, "reported running worker sessions")?;
    if running.saturating_add(1) > max_slots {
        return Err(error(
            OccupancyStoreErrorKind::CapacityExhausted,
            "client node has no free worker session slot",
        ));
    }
    // The holder must hold an active access grant with `use` permission.
    let permissions: Option<String> = transaction
        .query_row(
            "SELECT permissions FROM client_access_grants
             WHERE client_node_id = ?1 AND user_id = ?2 AND state = 'active'",
            params![claim.client_node_id(), claim.holder_user_id()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    let Some(permissions) = permissions else {
        return Err(error(
            OccupancyStoreErrorKind::AccessDenied,
            "holder has no active access grant on this client node",
        ));
    };
    let can_use = match permissions.as_str() {
        "use" | "use+manage" | "use+share" | "use+manage+share" => true,
        _ => {
            return Err(error(
                OccupancyStoreErrorKind::CorruptState,
                "stored access grant permissions are invalid",
            ));
        }
    };
    if !can_use {
        return Err(error(
            OccupancyStoreErrorKind::AccessDenied,
            "holder access grant does not include use",
        ));
    }
    // No active lease may exist. The partial unique index is the durable
    // backstop if a concurrent claim ever slips past this read.
    let active = transaction
        .query_row(
            "SELECT occupancy_lease_id FROM client_occupancy_leases
             WHERE client_node_id = ?1
               AND state IN ('reserving', 'occupied', 'draining', 'recovery_pending')",
            [claim.client_node_id()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    if active.is_some() {
        return Err(error(
            OccupancyStoreErrorKind::ActiveLeaseConflict,
            "client node already holds an active occupancy lease",
        ));
    }
    Ok(())
}

/// Mints the next fencing token inside the caller's transaction.
fn mint_fencing_token(transaction: &Transaction<'_>) -> Result<u64, OccupancyStoreError> {
    transaction
        .execute(
            "INSERT OR IGNORE INTO occupancy_fencing_tokens (singleton, last_issued_token)
             VALUES (1, 0)",
            [],
        )
        .map_err(|sql| sql_error(&sql))?;
    let current: i64 = transaction
        .query_row(
            "SELECT last_issued_token FROM occupancy_fencing_tokens WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))?;
    let current = from_sql_integer(current, "fencing token")?;
    if current >= MAX_SAFE_INTEGER {
        return Err(error(
            OccupancyStoreErrorKind::FencingTokenExhausted,
            "fencing token counter reached the safe integer ceiling",
        ));
    }
    let next = current + 1;
    let updated = transaction
        .execute(
            "UPDATE occupancy_fencing_tokens SET last_issued_token = ?2
             WHERE singleton = 1 AND last_issued_token = ?1",
            params![sql_integer(current)?, sql_integer(next)?],
        )
        .map_err(|sql| sql_error(&sql))?;
    if updated != 1 {
        return Err(cas_lost("fencing token mint"));
    }
    Ok(next)
}

/// Terminal `released` write guarded by the lease's current active state and
/// fencing token.
fn release_lease(
    transaction: &Transaction<'_>,
    occupancy_lease_id: &str,
    current: OccupancyLeaseState,
    fencing_token: u64,
    reason: OccupancyReleaseReason,
) -> Result<(), OccupancyStoreError> {
    let updated = transaction
        .execute(
            "UPDATE client_occupancy_leases
             SET state = 'released', release_reason = ?3, revision = revision + 1
             WHERE occupancy_lease_id = ?1
               AND state = ?2 AND fencing_token = ?4",
            params![
                occupancy_lease_id,
                current.as_str(),
                reason.as_str(),
                sql_integer(fencing_token)?
            ],
        )
        .map_err(|sql| sql_error(&sql))?;
    if updated != 1 {
        return Err(cas_lost("release"));
    }
    Ok(())
}

fn load_occupancy_lease(
    connection: &rusqlite::Connection,
    occupancy_lease_id: &str,
) -> Result<Option<OccupancyLeaseRecord>, OccupancyStoreError> {
    connection
        .query_row(
            "SELECT occupancy_lease_id, client_node_id, holder_user_id, fencing_token,
                    claim_request_id, state, acknowledged_at, claimed_at, last_renewed_at,
                    idle_expires_at, recovery_deadline_at, release_reason, created_at, revision
             FROM client_occupancy_leases WHERE occupancy_lease_id = ?1",
            [occupancy_lease_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, i64>(13)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(occupancy_lease_record_from_row)
        .transpose()
}

#[allow(clippy::type_complexity)]
fn occupancy_lease_record_from_row(
    row: (
        String,
        String,
        String,
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        i64,
    ),
) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
    let (
        occupancy_lease_id,
        client_node_id,
        holder_user_id,
        fencing_token,
        claim_request_id,
        state,
        acknowledged_at,
        claimed_at,
        last_renewed_at,
        idle_expires_at,
        recovery_deadline_at,
        release_reason,
        created_at,
        revision,
    ) = row;
    let parse_optional = |value: Option<String>,
                          label: &'static str|
     -> Result<Option<Instant>, OccupancyStoreError> {
        value
            .map(|value| parse_stored_instant(&value, label))
            .transpose()
    };
    Ok(OccupancyLeaseRecord {
        occupancy_lease_id,
        client_node_id,
        holder_user_id,
        fencing_token: from_sql_integer(fencing_token, "fencing token")?,
        claim_request_id,
        state: OccupancyLeaseState::parse(&state)?,
        acknowledged_at: parse_optional(acknowledged_at, "acknowledged at")?,
        claimed_at: parse_optional(claimed_at, "claimed at")?,
        last_renewed_at: parse_optional(last_renewed_at, "last renewed at")?,
        idle_expires_at: parse_optional(idle_expires_at, "idle expiry")?,
        recovery_deadline_at: parse_optional(recovery_deadline_at, "recovery deadline")?,
        release_reason: release_reason
            .map(|value| OccupancyReleaseReason::parse(&value))
            .transpose()?,
        created_at: parse_stored_instant(&created_at, "created at")?,
        revision: from_sql_integer(revision, "occupancy lease revision")?,
    })
}

fn validate_schema(connection: &rusqlite::Connection) -> Result<(), OccupancyStoreError> {
    validate_columns(
        connection,
        "client_occupancy_leases",
        &[
            "occupancy_lease_id",
            "client_node_id",
            "holder_user_id",
            "fencing_token",
            "claim_request_id",
            "state",
            "acknowledged_at",
            "claimed_at",
            "last_renewed_at",
            "idle_expires_at",
            "recovery_deadline_at",
            "release_reason",
            "created_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "occupancy_fencing_tokens",
        &["singleton", "last_issued_token"],
    )
}

fn validate_columns(
    connection: &rusqlite::Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), OccupancyStoreError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            OccupancyStoreErrorKind::CorruptState,
            "occupancy ledger schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_occupancy_lease_id(value: &str) -> Result<(), OccupancyStoreError> {
    validate_crockford_id(value, "ocl_", "occupancy lease id")
}

fn validate_client_node_id(value: &str) -> Result<(), OccupancyStoreError> {
    validate_crockford_id(value, "cnd_", "client node id")
}

fn validate_user_id(value: &str) -> Result<(), OccupancyStoreError> {
    validate_crockford_id(value, "usr_", "user id")
}

fn validate_claim_request_id(value: &str) -> Result<(), OccupancyStoreError> {
    validate_crockford_id(value, "req_", "claim request id")
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), OccupancyStoreError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(error(
            OccupancyStoreErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(error(
            OccupancyStoreErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ));
    }
    Ok(())
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

/// Validates the canonical `domain.Instant` shape (`YYYY-MM-DDTHH:MM:SS.sssZ`).
fn validate_instant(value: &Instant, label: &str) -> Result<(), OccupancyStoreError> {
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
            OccupancyStoreErrorKind::InvalidInput,
            format!("{label} instant is not canonical"),
        ))
    }
}

fn parse_stored_instant(value: &str, label: &str) -> Result<Instant, OccupancyStoreError> {
    let instant = Instant(value.to_owned());
    validate_instant(&instant, label).map(|()| instant)
}

fn validate_fencing_token(value: u64) -> Result<(), OccupancyStoreError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(error(
            OccupancyStoreErrorKind::InvalidInput,
            "fencing token is outside the durable range",
        ));
    }
    Ok(())
}

fn ensure_fencing_token(
    record: &OccupancyLeaseRecord,
    fencing_token: u64,
) -> Result<(), OccupancyStoreError> {
    if record.fencing_token != fencing_token {
        return Err(error(
            OccupancyStoreErrorKind::FencingTokenMismatch,
            "occupancy fencing token does not match the lease",
        ));
    }
    Ok(())
}

fn ensure_replay_reason(
    record: &OccupancyLeaseRecord,
    reason: OccupancyReleaseReason,
) -> Result<(), OccupancyStoreError> {
    if record.release_reason == Some(reason) {
        return Ok(());
    }
    Err(illegal_transition(record, "replay"))
}

fn illegal_transition(record: &OccupancyLeaseRecord, action: &str) -> OccupancyStoreError {
    error(
        OccupancyStoreErrorKind::IllegalStateTransition,
        format!(
            "occupancy transition {} during {action} is not legal",
            record.state
        ),
    )
}

fn map_lease_insert_sql(sql: &rusqlite::Error) -> OccupancyStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            // The realistic unique violation is the partial one-active-per-
            // client index; the fencing-token unique shares the extended code
            // and is unreachable while the allocator mints inside the same
            // transaction, so both fail closed as an active lease conflict.
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                OccupancyStoreErrorKind::ActiveLeaseConflict,
                "client node already holds an active occupancy lease",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                OccupancyStoreErrorKind::OccupancyLeaseConflict,
                "occupancy lease id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => unknown_client_node(),
            _ => error(
                OccupancyStoreErrorKind::InvalidInput,
                "occupancy lease violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn lease_missing_after_write() -> OccupancyStoreError {
    error(
        OccupancyStoreErrorKind::CorruptState,
        "occupancy lease row is missing after the write",
    )
}

/// Loads one lease or fails with the unknown-lease category.
fn require_occupancy_lease(
    connection: &rusqlite::Connection,
    occupancy_lease_id: &str,
) -> Result<OccupancyLeaseRecord, OccupancyStoreError> {
    load_occupancy_lease(connection, occupancy_lease_id)?.ok_or_else(|| {
        error(
            OccupancyStoreErrorKind::UnknownOccupancyLease,
            "occupancy lease does not exist",
        )
    })
}

fn unknown_client_node() -> OccupancyStoreError {
    error(
        OccupancyStoreErrorKind::UnknownClientNode,
        "client node does not exist",
    )
}

fn cas_lost(action: &str) -> OccupancyStoreError {
    error(
        OccupancyStoreErrorKind::RevisionConflict,
        format!("occupancy compare-and-swap lost during {action}"),
    )
}

fn sql_integer(value: u64) -> Result<i64, OccupancyStoreError> {
    i64::try_from(value).map_err(|_| {
        error(
            OccupancyStoreErrorKind::InvalidInput,
            "numeric value exceeds the SQLite integer range",
        )
    })
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, OccupancyStoreError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            OccupancyStoreErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            OccupancyStoreErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn storage_error(storage: &StorageError) -> OccupancyStoreError {
    error(
        OccupancyStoreErrorKind::Storage,
        format!("occupancy ledger storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> OccupancyStoreError {
    error(
        OccupancyStoreErrorKind::Storage,
        "occupancy ledger storage operation failed",
    )
}

fn error(kind: OccupancyStoreErrorKind, message: impl Into<String>) -> OccupancyStoreError {
    OccupancyStoreError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn validates_canonical_identities() {
        assert!(
            OccupancyClaim::try_new(
                "ocl_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "cnd_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "req_AAAAAAAAAAAAAAAAAAAAAAAAA1"
            )
            .is_ok()
        );
        assert!(
            OccupancyClaim::try_new(
                "lease_1",
                "cnd_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "req_AAAAAAAAAAAAAAAAAAAAAAAAA1"
            )
            .is_err()
        );
        assert!(
            OccupancyClaim::try_new(
                "ocl_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "node_1",
                "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "req_AAAAAAAAAAAAAAAAAAAAAAAAA1"
            )
            .is_err()
        );
        assert!(
            OccupancyClaim::try_new(
                "ocl_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "cnd_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "anonymous",
                "req_AAAAAAAAAAAAAAAAAAAAAAAAA1"
            )
            .is_err()
        );
        assert!(
            OccupancyClaim::try_new(
                "ocl_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "cnd_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "claim-1"
            )
            .is_err()
        );
    }

    #[test]
    fn active_states_match_the_frozen_active_set() {
        assert!(OccupancyLeaseState::Reserving.is_active());
        assert!(OccupancyLeaseState::Occupied.is_active());
        assert!(OccupancyLeaseState::Draining.is_active());
        assert!(OccupancyLeaseState::RecoveryPending.is_active());
        assert!(!OccupancyLeaseState::Released.is_active());
        assert!(!OccupancyLeaseState::Expired.is_active());
    }

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        // Wall-clock nanos keep the directory unique even when the operating
        // system reuses a previous run's process id.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-client-occupancy-unit-{name}-{}-{suffix}-{nanos}",
            std::process::id()
        ))
    }

    fn unit_instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn seed_online_node(storage: &mut SqliteStorage, seed: u64) -> String {
        let registration = crate::ClientNodeRegistration::try_new(
            format!("cnd_{seed:026}"),
            format!("{seed:012}"),
            format!("Device {seed}"),
            "aarch64-apple-darwin",
            "aarch64",
            "1.2.3",
            None,
            Some(format!("cix_{seed:026}")),
            2,
        )
        .expect("registration");
        let client = format!("cnd_{seed:026}");
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .register(&registration, 0, &unit_instant("2026-01-01T00:00:00.000Z"))
            .expect("register");
        let online = registry
            .update_presence(client.as_str(), crate::ClientPresenceState::Online, 1)
            .expect("presence online");
        assert_eq!(online.presence_state, crate::ClientPresenceState::Online);
        client
    }

    #[test]
    fn partial_unique_index_backstops_the_single_active_lease() {
        let mut storage =
            SqliteStorage::open(temporary_directory("partial-index")).expect("storage");
        let client = seed_online_node(&mut storage, 1);
        let (first_token, second_token, third_token) = {
            let mut ledger = storage.client_occupancy_ledger().expect("ledger");
            let first = ledger.mint_fencing_token().expect("mint");
            let second = ledger.mint_fencing_token().expect("mint");
            let third = ledger.mint_fencing_token().expect("mint");
            (first, second, third)
        };
        let (lease_one, lease_two, lease_three) = (1_u64, 2_u64, 3_u64);
        let first = format!("ocl_{lease_one:026}");
        let inserted = storage
            .connection_mut()
            .expect("connection")
            .execute(
                "INSERT INTO client_occupancy_leases
                 (occupancy_lease_id, client_node_id, holder_user_id, fencing_token,
                  claim_request_id, state, created_at, revision)
                 VALUES (?1, ?2, 'usr_AAAAAAAAAAAAAAAAAAAAAAAA1', ?3,
                         'req_AAAAAAAAAAAAAAAAAAAAAAAA1', 'reserving',
                         '2026-01-01T00:00:00.000Z', 1)",
                rusqlite::params![first, client, i64::try_from(first_token).expect("token")],
            )
            .expect("first active lease insert");
        assert_eq!(inserted, 1);

        // A second active row on the same client must hit the partial unique
        // index even though the ledger pre-check was bypassed.
        let second = format!("ocl_{lease_two:026}");
        let conflict = storage
            .connection_mut()
            .expect("connection")
            .execute(
                "INSERT INTO client_occupancy_leases
                 (occupancy_lease_id, client_node_id, holder_user_id, fencing_token,
                  claim_request_id, state, created_at, revision)
                 VALUES (?1, ?2, 'usr_AAAAAAAAAAAAAAAAAAAAAAAA2', ?3,
                         'req_AAAAAAAAAAAAAAAAAAAAAAAA2', 'occupied',
                         '2026-01-01T00:00:00.000Z', 1)",
                rusqlite::params![second, client, i64::try_from(second_token).expect("token")],
            )
            .expect_err("the partial unique index must refuse a second active lease");
        assert!(matches!(
            conflict,
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::ConstraintViolation,
                    ..
                },
                _
            )
        ));

        // After the active row terminates, the same client accepts a new
        // active lease: history rows never block future occupancy.
        storage
            .connection_mut()
            .expect("connection")
            .execute(
                "UPDATE client_occupancy_leases
                 SET state = 'released', release_reason = 'ack_timeout', revision = revision + 1
                 WHERE occupancy_lease_id = ?1",
                [first.as_str()],
            )
            .expect("terminal update");
        let third = format!("ocl_{lease_three:026}");
        let inserted = storage
            .connection_mut()
            .expect("connection")
            .execute(
                "INSERT INTO client_occupancy_leases
                 (occupancy_lease_id, client_node_id, holder_user_id, fencing_token,
                  claim_request_id, state, created_at, revision)
                 VALUES (?1, ?2, 'usr_AAAAAAAAAAAAAAAAAAAAAAAA2', ?3,
                         'req_AAAAAAAAAAAAAAAAAAAAAAAA2', 'occupied',
                         '2026-01-01T00:01:00.000Z', 1)",
                rusqlite::params![third, client, i64::try_from(third_token).expect("token")],
            )
            .expect("active lease after terminal history");
        assert_eq!(inserted, 1);
        // The allocator never reissued a token across the whole sequence.
        let ledger = storage.client_occupancy_ledger().expect("ledger");
        assert_eq!(
            ledger.current_fencing_token().expect("current"),
            third_token
        );
    }

    #[test]
    fn claim_gate_refuses_a_node_that_stops_accepting() {
        let mut storage =
            SqliteStorage::open(temporary_directory("not-accepting")).expect("storage");
        let client = seed_online_node(&mut storage, 2);
        {
            let issuance = crate::AccessGrantIssuance::try_new(
                "cag_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                client.as_str(),
                "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                crate::GrantTrustMode::Trusted,
                None,
            )
            .expect("issuance");
            storage
                .client_connect_ledger()
                .expect("connect ledger")
                .create_grant(
                    &issuance,
                    crate::GrantSource::Administrator,
                    crate::GrantPermissions::USE,
                    &unit_instant("2026-01-01T00:00:00.000Z"),
                )
                .expect("grant");
        }
        // Flip the independent accepting-connections switch off through the
        // durable projection (no public registry setter exists for it).
        storage
            .connection_mut()
            .expect("connection")
            .execute(
                "UPDATE client_nodes SET accepting_connections = 0 WHERE client_node_id = ?1",
                [client.as_str()],
            )
            .expect("accepting switch");
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        let error = ledger
            .atomic_claim(
                &OccupancyClaim::try_new(
                    "ocl_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                    client.as_str(),
                    "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                    "req_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                )
                .expect("claim"),
                &unit_instant("2026-01-01T00:01:00.000Z"),
            )
            .expect_err("a not-accepting node must refuse occupancy");
        assert_eq!(
            error.kind(),
            OccupancyStoreErrorKind::NotAcceptingConnections
        );
    }

    #[test]
    fn claim_gate_refuses_a_locked_node() {
        let mut storage = SqliteStorage::open(temporary_directory("locked")).expect("storage");
        let client = seed_online_node(&mut storage, 3);
        {
            let issuance = crate::AccessGrantIssuance::try_new(
                "cag_AAAAAAAAAAAAAAAAAAAAAAAAA2",
                client.as_str(),
                "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                crate::GrantTrustMode::Trusted,
                None,
            )
            .expect("issuance");
            storage
                .client_connect_ledger()
                .expect("connect ledger")
                .create_grant(
                    &issuance,
                    crate::GrantSource::Administrator,
                    crate::GrantPermissions::USE,
                    &unit_instant("2026-01-01T00:00:00.000Z"),
                )
                .expect("grant");
        }
        // Flip the independent machine-level lock switch while the presence
        // stays `online` (no public registry setter exists for it).
        storage
            .connection_mut()
            .expect("connection")
            .execute(
                "UPDATE client_nodes SET lock_state = 'locked' WHERE client_node_id = ?1",
                [client.as_str()],
            )
            .expect("lock switch");
        let mut ledger = storage.client_occupancy_ledger().expect("ledger");
        let error = ledger
            .atomic_claim(
                &OccupancyClaim::try_new(
                    "ocl_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                    client.as_str(),
                    "usr_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                    "req_AAAAAAAAAAAAAAAAAAAAAAAAA1",
                )
                .expect("claim"),
                &unit_instant("2026-01-01T00:01:00.000Z"),
            )
            .expect_err("a locked node must refuse occupancy");
        assert_eq!(error.kind(), OccupancyStoreErrorKind::ClientLocked);
    }
}
