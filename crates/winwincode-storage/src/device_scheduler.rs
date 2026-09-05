// SPDX-License-Identifier: Apache-2.0

//! Device scheduler slot-reservation ledger over the sole product-state
//! `SQLite` authority (plan FLOW-100.2).
//!
//! The two-phase scheduler reserves one free worker-session slot of a client
//! node durably before any Worker request is issued, and the live
//! `WorkerLaunchGrant` takes the slot accounting over once it exists. The
//! durable capacity view therefore stays one number:
//! `pending reservations + non-terminal launch grants` may never exceed the
//! client's `max_concurrent_worker_sessions`, judged inside one immediate
//! transaction so concurrent schedulers can never oversell.
//!
//! Every reservation is keyed by the caller's canonical `req_` request
//! identity. A repeated request with a byte-identical command replays the
//! stored outcome without a second row; a reused request identity with a
//! different body is refused with `RequestConflict`. The fixed
//! compare-and-swap rules make every transition at-most-once idempotent:
//! `reserved -> granted` stamps exactly one launch grant, and
//! `reserved -> released` frees the slot for the failure paths (launch
//! failure, timeout rollback, and the stale-reservation sweep that reclaims
//! reservations a crashed scheduler never settled).

use std::fmt;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;

const DEVICE_SCHEDULER_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS device_scheduler_reservations (
    device_scheduler_reservation_id TEXT PRIMARY KEY NOT NULL,
    request_id TEXT NOT NULL UNIQUE,
    request_digest TEXT NOT NULL,
    client_node_id TEXT NOT NULL,
    holder_user_id TEXT NOT NULL,
    occupancy_lease_id TEXT NOT NULL,
    occupancy_fencing_token INTEGER NOT NULL
        CHECK (occupancy_fencing_token > 0 AND occupancy_fencing_token <= 9007199254740991),
    repository_binding_id TEXT NOT NULL,
    worker_session_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('reserved', 'granted', 'released')),
    launch_grant_id TEXT,
    release_reason TEXT CHECK (release_reason IS NULL OR release_reason IN (
        'launch_failed', 'rolled_back', 'expired')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT,
    FOREIGN KEY (repository_binding_id)
        REFERENCES repository_bindings(repository_binding_id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX IF NOT EXISTS device_scheduler_reservations_one_per_request
    ON device_scheduler_reservations (request_id);
CREATE INDEX IF NOT EXISTS device_scheduler_reservations_by_client
    ON device_scheduler_reservations (client_node_id, state);
CREATE INDEX IF NOT EXISTS device_scheduler_reservations_by_session
    ON device_scheduler_reservations (worker_session_id, state);
";

/// Expected column layout used to refuse an incompatible existing schema.
const DEVICE_SCHEDULER_COLUMNS: [&str; 15] = [
    "device_scheduler_reservation_id",
    "request_id",
    "request_digest",
    "client_node_id",
    "holder_user_id",
    "occupancy_lease_id",
    "occupancy_fencing_token",
    "repository_binding_id",
    "worker_session_id",
    "state",
    "launch_grant_id",
    "release_reason",
    "created_at",
    "updated_at",
    "revision",
];

/// Lifecycle state of one scheduler slot reservation (plan FLOW-100.2).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceSchedulerReservationState {
    /// Phase one committed; the slot is held until the Worker request is
    /// issued or the reservation is released.
    Reserved,
    /// Phase two issued its launch grant; the grant ledger owns the slot.
    Granted,
    /// Terminal: the reservation was released before any grant existed.
    Released,
}

impl DeviceSchedulerReservationState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Granted => "granted",
            Self::Released => "released",
        }
    }

    fn parse(value: &str) -> Result<Self, DeviceSchedulerStoreError> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "granted" => Ok(Self::Granted),
            "released" => Ok(Self::Released),
            _ => Err(error(
                DeviceSchedulerStoreErrorKind::CorruptState,
                "stored device scheduler reservation state is invalid",
            )),
        }
    }
}

/// Why a `reserved` slot was freed without a launch grant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceSchedulerReleaseReason {
    /// Phase two refused the Worker request; the slot is freed immediately.
    LaunchFailed,
    /// The holder or the bounded launch flow rolled the attempt back.
    RolledBack,
    /// The stale-reservation sweep reclaimed a never-settled reservation.
    Expired,
}

impl DeviceSchedulerReleaseReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LaunchFailed => "launch_failed",
            Self::RolledBack => "rolled_back",
            Self::Expired => "expired",
        }
    }

    fn parse(value: &str) -> Result<Self, DeviceSchedulerStoreError> {
        match value {
            "launch_failed" => Ok(Self::LaunchFailed),
            "rolled_back" => Ok(Self::RolledBack),
            "expired" => Ok(Self::Expired),
            _ => Err(error(
                DeviceSchedulerStoreErrorKind::CorruptState,
                "stored device scheduler release reason is invalid",
            )),
        }
    }
}

/// Validated phase-one command: atomically reserve one free worker-session
/// slot of a client node (plan FLOW-100.2).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceSchedulerReservationRequest {
    device_scheduler_reservation_id: String,
    request_id: String,
    client_node_id: String,
    holder_user_id: String,
    occupancy_lease_id: String,
    occupancy_fencing_token: u64,
    repository_binding_id: String,
    worker_session_id: String,
}

impl DeviceSchedulerReservationRequest {
    /// Builds one validated reservation command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or a fencing token outside the
    /// durable range.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        device_scheduler_reservation_id: impl Into<String>,
        request_id: impl Into<String>,
        client_node_id: impl Into<String>,
        holder_user_id: impl Into<String>,
        occupancy_lease_id: impl Into<String>,
        occupancy_fencing_token: u64,
        repository_binding_id: impl Into<String>,
        worker_session_id: impl Into<String>,
    ) -> Result<Self, DeviceSchedulerStoreError> {
        let command = Self {
            device_scheduler_reservation_id: device_scheduler_reservation_id.into(),
            request_id: request_id.into(),
            client_node_id: client_node_id.into(),
            holder_user_id: holder_user_id.into(),
            occupancy_lease_id: occupancy_lease_id.into(),
            occupancy_fencing_token,
            repository_binding_id: repository_binding_id.into(),
            worker_session_id: worker_session_id.into(),
        };
        validate_crockford_id(
            &command.device_scheduler_reservation_id,
            "dsr_",
            "device scheduler reservation id",
        )?;
        validate_crockford_id(&command.request_id, "req_", "request id")?;
        validate_crockford_id(&command.client_node_id, "cnd_", "client node id")?;
        validate_crockford_id(&command.holder_user_id, "usr_", "holder user id")?;
        validate_crockford_id(&command.occupancy_lease_id, "ocl_", "occupancy lease id")?;
        validate_fencing_token(command.occupancy_fencing_token)?;
        validate_crockford_id(
            &command.repository_binding_id,
            "rbd_",
            "repository binding id",
        )?;
        validate_crockford_id(&command.worker_session_id, "ws_", "worker session id")?;
        Ok(command)
    }

    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn worker_session_id(&self) -> &str {
        &self.worker_session_id
    }
}

/// Validated phase-two settlement: stamp the launch grant that took the
/// reserved slot over (`reserved -> granted`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceSchedulerReservationGrant {
    device_scheduler_reservation_id: String,
    launch_grant_id: String,
    expected_revision: u64,
}

impl DeviceSchedulerReservationGrant {
    /// Builds one validated grant settlement command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or an out-of-range revision.
    pub fn try_new(
        device_scheduler_reservation_id: impl Into<String>,
        launch_grant_id: impl Into<String>,
        expected_revision: u64,
    ) -> Result<Self, DeviceSchedulerStoreError> {
        let command = Self {
            device_scheduler_reservation_id: device_scheduler_reservation_id.into(),
            launch_grant_id: launch_grant_id.into(),
            expected_revision,
        };
        validate_crockford_id(
            &command.device_scheduler_reservation_id,
            "dsr_",
            "device scheduler reservation id",
        )?;
        validate_crockford_id(&command.launch_grant_id, "wlg_", "launch grant id")?;
        validate_revision(command.expected_revision)?;
        Ok(command)
    }
}

/// Validated release command: free a `reserved` slot without a grant
/// (`reserved -> released`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceSchedulerReservationRelease {
    device_scheduler_reservation_id: String,
    expected_revision: u64,
    reason: DeviceSchedulerReleaseReason,
}

impl DeviceSchedulerReservationRelease {
    /// Builds one validated release command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or an out-of-range revision.
    pub fn try_new(
        device_scheduler_reservation_id: impl Into<String>,
        expected_revision: u64,
        reason: DeviceSchedulerReleaseReason,
    ) -> Result<Self, DeviceSchedulerStoreError> {
        let command = Self {
            device_scheduler_reservation_id: device_scheduler_reservation_id.into(),
            expected_revision,
            reason,
        };
        validate_crockford_id(
            &command.device_scheduler_reservation_id,
            "dsr_",
            "device scheduler reservation id",
        )?;
        validate_revision(command.expected_revision)?;
        Ok(command)
    }
}

/// Complete durable reservation record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceSchedulerReservationRecord {
    pub device_scheduler_reservation_id: String,
    pub request_id: String,
    pub client_node_id: String,
    pub holder_user_id: String,
    pub occupancy_lease_id: String,
    pub occupancy_fencing_token: u64,
    pub repository_binding_id: String,
    pub worker_session_id: String,
    pub state: DeviceSchedulerReservationState,
    pub launch_grant_id: Option<String>,
    pub release_reason: Option<DeviceSchedulerReleaseReason>,
    pub created_at: Instant,
    pub updated_at: Instant,
    pub revision: u64,
}

/// Outcome of one phase-one reserve command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceSchedulerReserveOutcome {
    /// A new `reserved` row committed; the slot is held for this scheduler.
    Reserved(Box<DeviceSchedulerReservationRecord>),
    /// The same request identity replayed with a byte-identical command; the
    /// stored reservation is returned unchanged and idempotently.
    Replayed(Box<DeviceSchedulerReservationRecord>),
}

/// Stable device scheduler ledger failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceSchedulerStoreErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// The client node has no free worker-session slot.
    CapacityExhausted,
    /// No reservation matches the requested identity.
    UnknownReservation,
    /// A request identity was reused with a different body.
    RequestConflict,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost a race.
    RevisionConflict,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free device scheduler ledger error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSchedulerStoreError {
    kind: DeviceSchedulerStoreErrorKind,
    message: String,
}

impl DeviceSchedulerStoreError {
    #[must_use]
    pub const fn kind(&self) -> DeviceSchedulerStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for DeviceSchedulerStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DeviceSchedulerStoreError {}

/// Device scheduler reservation ledger borrowing the sole product-state
/// `SQLite` authority.
pub struct DeviceSchedulerLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable device scheduler reservation ledger on this same
    /// product-state database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
    pub fn device_scheduler_reservation_ledger(
        &mut self,
    ) -> Result<DeviceSchedulerLedger<'_>, DeviceSchedulerStoreError> {
        DeviceSchedulerLedger::new(self)
    }
}

impl<'storage> DeviceSchedulerLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, DeviceSchedulerStoreError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(DEVICE_SCHEDULER_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Atomically reserves one free worker-session slot of a client node
    /// (plan FLOW-100.2 phase one).
    ///
    /// Inside one immediate transaction the gate reuses the durable capacity
    /// facts: the client node must exist, and its pending scheduler
    /// reservations plus its non-terminal launch grants (reconciled with the
    /// device-reported running count) must stay below
    /// `max_concurrent_worker_sessions`. The request identity replays a
    /// stored reservation byte-identically and refuses a different body.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, an exhausted capacity, a reused
    /// request identity with a different body, or storage failure.
    pub fn reserve(
        &mut self,
        command: &DeviceSchedulerReservationRequest,
        now: &Instant,
    ) -> Result<DeviceSchedulerReserveOutcome, DeviceSchedulerStoreError> {
        validate_instant(now, "reserve time")?;
        let request_digest = command_digest(&command.fingerprint())?;
        let transaction = self.transaction()?;
        if let Some(record) = stored_reservation_by_request(&transaction, &command.request_id)? {
            if record.request_digest != request_digest {
                return Err(error(
                    DeviceSchedulerStoreErrorKind::RequestConflict,
                    "device scheduler request id was reused with a different body",
                ));
            }
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(DeviceSchedulerReserveOutcome::Replayed(Box::new(
                record.record,
            )));
        }
        ensure_free_capacity(&transaction, &command.client_node_id)?;
        let inserted = transaction
            .execute(
                "INSERT INTO device_scheduler_reservations
                 (device_scheduler_reservation_id, request_id, request_digest,
                  client_node_id, holder_user_id, occupancy_lease_id,
                  occupancy_fencing_token, repository_binding_id, worker_session_id,
                  state, launch_grant_id, release_reason, created_at, updated_at,
                  revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'reserved', NULL,
                         NULL, ?10, ?10, 1)",
                params![
                    command.device_scheduler_reservation_id,
                    command.request_id,
                    request_digest,
                    command.client_node_id,
                    command.holder_user_id,
                    command.occupancy_lease_id,
                    sql_integer(command.occupancy_fencing_token)?,
                    command.repository_binding_id,
                    command.worker_session_id,
                    now.0,
                ],
            )
            .map_err(|sql| map_reservation_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                DeviceSchedulerStoreErrorKind::Storage,
                "device scheduler reservation insert did not store exactly one row",
            ));
        }
        let record = load_reservation(&transaction, &command.device_scheduler_reservation_id)?
            .ok_or_else(reservation_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(DeviceSchedulerReserveOutcome::Reserved(Box::new(record)))
    }

    /// Stamps the launch grant that took the reserved slot over
    /// (`reserved -> granted`, plan FLOW-100.2 phase two). Re-stamping the
    /// same grant id is an idempotent no-op so a retried settlement after a
    /// crash cannot fork the slot accounting.
    ///
    /// # Errors
    ///
    /// Rejects an unknown reservation, a lost revision race, a different
    /// grant already stamped, or storage failure.
    pub fn settle_granted(
        &mut self,
        command: &DeviceSchedulerReservationGrant,
        now: &Instant,
    ) -> Result<DeviceSchedulerReservationRecord, DeviceSchedulerStoreError> {
        validate_instant(now, "grant settlement time")?;
        let transaction = self.transaction()?;
        let stored = stored_reservation(&transaction, &command.device_scheduler_reservation_id)?;
        let Some(stored) = stored else {
            return Err(unknown_reservation(
                &command.device_scheduler_reservation_id,
            ));
        };
        if let Some(stamped) = &stored.record.launch_grant_id {
            if *stamped == command.launch_grant_id {
                transaction.commit().map_err(|sql| sql_error(&sql))?;
                return Ok(stored.record);
            }
            return Err(illegal_transition(
                &stored.record,
                "the reservation already carries another launch grant",
            ));
        }
        let updated = transaction
            .execute(
                "UPDATE device_scheduler_reservations
                 SET state = 'granted', launch_grant_id = ?2, updated_at = ?3,
                     revision = revision + 1
                 WHERE device_scheduler_reservation_id = ?1 AND revision = ?4
                   AND state = 'reserved'",
                params![
                    command.device_scheduler_reservation_id,
                    command.launch_grant_id,
                    now.0,
                    sql_integer(command.expected_revision)?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("device scheduler grant settlement"));
        }
        let record = load_reservation(&transaction, &command.device_scheduler_reservation_id)?
            .ok_or_else(reservation_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Frees a `reserved` slot without a grant (`reserved -> released`,
    /// plan FLOW-100.2 failure paths). Re-releasing with the same reason is
    /// an idempotent no-op.
    ///
    /// # Errors
    ///
    /// Rejects an unknown reservation, a lost revision race, a `granted`
    /// reservation, a conflicting terminal release, or storage failure.
    pub fn release(
        &mut self,
        command: &DeviceSchedulerReservationRelease,
        now: &Instant,
    ) -> Result<DeviceSchedulerReservationRecord, DeviceSchedulerStoreError> {
        validate_instant(now, "release time")?;
        let transaction = self.transaction()?;
        let stored = stored_reservation(&transaction, &command.device_scheduler_reservation_id)?;
        let Some(stored) = stored else {
            return Err(unknown_reservation(
                &command.device_scheduler_reservation_id,
            ));
        };
        if stored.record.state == DeviceSchedulerReservationState::Released {
            if stored.terminal_release_reason == Some(command.reason) {
                transaction.commit().map_err(|sql| sql_error(&sql))?;
                return Ok(stored.record);
            }
            return Err(illegal_transition(
                &stored.record,
                "the reservation already terminated with another release reason",
            ));
        }
        let updated = transaction
            .execute(
                "UPDATE device_scheduler_reservations
                 SET state = 'released', release_reason = ?2, updated_at = ?3,
                     revision = revision + 1
                 WHERE device_scheduler_reservation_id = ?1 AND revision = ?4
                   AND state = 'reserved'",
                params![
                    command.device_scheduler_reservation_id,
                    command.reason.as_str(),
                    now.0,
                    sql_integer(command.expected_revision)?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("device scheduler reservation release"));
        }
        let record = load_reservation(&transaction, &command.device_scheduler_reservation_id)?
            .ok_or_else(reservation_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Reclaims every `reserved` row created before `cutoff` (plan FLOW-100.2
    /// crash safety): a scheduler that died between the reservation and the
    /// Worker request must not hold the slot forever. Returns the released
    /// reservation ids.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn expire_stale(
        &mut self,
        cutoff: &Instant,
    ) -> Result<Vec<String>, DeviceSchedulerStoreError> {
        validate_instant(cutoff, "stale cutoff")?;
        let transaction = self.transaction()?;
        let stale: Vec<(String, i64)> = {
            let mut statement = transaction
                .prepare(
                    "SELECT device_scheduler_reservation_id, revision
                     FROM device_scheduler_reservations
                     WHERE state = 'reserved' AND created_at < ?1
                     ORDER BY created_at",
                )
                .map_err(|sql| sql_error(&sql))?;
            statement
                .query_map([cutoff.0.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|sql| sql_error(&sql))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|sql| sql_error(&sql))?
        };
        let mut released = Vec::with_capacity(stale.len());
        for (reservation_id, revision) in stale {
            let updated = transaction
                .execute(
                    "UPDATE device_scheduler_reservations
                     SET state = 'released', release_reason = 'expired',
                         updated_at = ?2, revision = revision + 1
                     WHERE device_scheduler_reservation_id = ?1 AND revision = ?3
                       AND state = 'reserved'",
                    params![reservation_id, cutoff.0, revision],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated == 1 {
                released.push(reservation_id);
            }
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(released)
    }

    /// Returns one durable reservation by its own identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical reservation identity or storage failure.
    pub fn snapshot(
        &mut self,
        device_scheduler_reservation_id: &str,
    ) -> Result<Option<DeviceSchedulerReservationRecord>, DeviceSchedulerStoreError> {
        validate_crockford_id(
            device_scheduler_reservation_id,
            "dsr_",
            "device scheduler reservation id",
        )?;
        let connection = self.connection()?;
        let stored = stored_reservation(connection, device_scheduler_reservation_id)?;
        Ok(stored.map(|stored| stored.record))
    }

    /// Returns the durable reservation of one request identity, if any.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical request identity or storage failure.
    pub fn snapshot_by_request(
        &mut self,
        request_id: &str,
    ) -> Result<Option<DeviceSchedulerReservationRecord>, DeviceSchedulerStoreError> {
        validate_crockford_id(request_id, "req_", "request id")?;
        let connection = self.connection()?;
        let stored = stored_reservation_by_request(connection, request_id)?;
        Ok(stored.map(|stored| stored.record))
    }

    /// Counts the pending (`reserved`) slot reservations of one client node;
    /// the durable number the capacity CAS is judged against alongside the
    /// non-terminal launch grants.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or storage failure.
    pub fn pending_count_for_node(
        &mut self,
        client_node_id: &str,
    ) -> Result<u64, DeviceSchedulerStoreError> {
        validate_crockford_id(client_node_id, "cnd_", "client node id")?;
        let connection = self.connection()?;
        let stored: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM device_scheduler_reservations
                 WHERE client_node_id = ?1 AND state = 'reserved'",
                [client_node_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|sql| sql_error(&sql))?;
        from_sql_integer(stored, "pending reservation count")
    }

    fn transaction(&mut self) -> Result<rusqlite::Transaction<'_>, DeviceSchedulerStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&Connection, DeviceSchedulerStoreError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

/// Internal projection with the request digest the replay gate compares and
/// the parsed terminal release reason the idempotent re-release gate checks.
struct StoredReservation {
    record: DeviceSchedulerReservationRecord,
    request_digest: String,
    terminal_release_reason: Option<DeviceSchedulerReleaseReason>,
}

/// Authoritative reservation projection the ledger loads and replays.
const RESERVATION_SELECT: &str = "SELECT device_scheduler_reservation_id, request_id,
        request_digest, client_node_id, holder_user_id, occupancy_lease_id,
        occupancy_fencing_token, repository_binding_id, worker_session_id,
        state, launch_grant_id, release_reason, created_at, updated_at, revision
 FROM device_scheduler_reservations";

#[allow(clippy::type_complexity)]
fn reservation_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    i64,
)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
    ))
}

#[allow(clippy::type_complexity)]
fn complete_stored_reservation(
    row: (
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        i64,
    ),
) -> Result<StoredReservation, DeviceSchedulerStoreError> {
    let (
        device_scheduler_reservation_id,
        request_id,
        request_digest,
        client_node_id,
        holder_user_id,
        occupancy_lease_id,
        occupancy_fencing_token,
        repository_binding_id,
        worker_session_id,
        state,
        launch_grant_id,
        release_reason,
        created_at,
        updated_at,
        revision,
    ) = row;
    let terminal_release_reason = release_reason
        .map(|reason| DeviceSchedulerReleaseReason::parse(&reason))
        .transpose()?;
    Ok(StoredReservation {
        record: DeviceSchedulerReservationRecord {
            device_scheduler_reservation_id,
            request_id,
            client_node_id,
            holder_user_id,
            occupancy_lease_id,
            occupancy_fencing_token: from_sql_integer(
                occupancy_fencing_token,
                "occupancy fencing token",
            )?,
            repository_binding_id,
            worker_session_id,
            state: DeviceSchedulerReservationState::parse(&state)?,
            launch_grant_id,
            release_reason: terminal_release_reason,
            created_at: Instant(created_at),
            updated_at: Instant(updated_at),
            revision: from_sql_integer(revision, "reservation revision")?,
        },
        request_digest,
        terminal_release_reason,
    })
}

/// The request semantics the replay digest fingerprints. The internal
/// reservation identity is deliberately excluded: it is derived from the
/// request identity, so a replayed request must fingerprint identically no
/// matter which caller minted the reservation row.
#[derive(Deserialize, Serialize)]
struct RequestFingerprint<'command> {
    request_id: &'command str,
    client_node_id: &'command str,
    holder_user_id: &'command str,
    occupancy_lease_id: &'command str,
    occupancy_fencing_token: u64,
    repository_binding_id: &'command str,
    worker_session_id: &'command str,
}

impl DeviceSchedulerReservationRequest {
    fn fingerprint(&self) -> RequestFingerprint<'_> {
        RequestFingerprint {
            request_id: &self.request_id,
            client_node_id: &self.client_node_id,
            holder_user_id: &self.holder_user_id,
            occupancy_lease_id: &self.occupancy_lease_id,
            occupancy_fencing_token: self.occupancy_fencing_token,
            repository_binding_id: &self.repository_binding_id,
            worker_session_id: &self.worker_session_id,
        }
    }
}

/// The one durable capacity gate of phase one: pending scheduler
/// reservations plus the non-terminal launch grant count (reconciled with
/// the device-reported running count) must stay below the client's
/// `max_concurrent_worker_sessions`.
fn ensure_free_capacity(
    transaction: &Connection,
    client_node_id: &str,
) -> Result<(), DeviceSchedulerStoreError> {
    let node: Option<(i64, i64)> = transaction
        .query_row(
            "SELECT max_concurrent_worker_sessions, reported_running_worker_sessions
             FROM client_nodes WHERE client_node_id = ?1",
            [client_node_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    let Some((max_slots, reported_running)) = node else {
        return Err(error(
            DeviceSchedulerStoreErrorKind::UnknownClientNode,
            "no client node matches the reservation identity",
        ));
    };
    let max_worker_sessions = from_sql_integer(max_slots, "client worker session capacity")?;
    let reported_running = from_sql_integer(reported_running, "reported running worker sessions")?;
    let pending: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM device_scheduler_reservations
             WHERE client_node_id = ?1 AND state = 'reserved'",
            [client_node_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))?;
    let grants: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM worker_launch_grants
             WHERE client_node_id = ?1 AND state IN ('issued', 'consumed')",
            [client_node_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|sql| sql_error(&sql))?;
    let pending = from_sql_integer(pending, "pending reservation count")?;
    let granted = from_sql_integer(grants, "reserved worker session count")?;
    let in_flight = reported_running.max(granted).saturating_add(pending);
    if in_flight >= max_worker_sessions {
        return Err(error(
            DeviceSchedulerStoreErrorKind::CapacityExhausted,
            "the client node has no free worker-session slot to reserve",
        ));
    }
    Ok(())
}

fn stored_reservation(
    connection: &Connection,
    device_scheduler_reservation_id: &str,
) -> Result<Option<StoredReservation>, DeviceSchedulerStoreError> {
    connection
        .query_row(
            &format!("{RESERVATION_SELECT} WHERE device_scheduler_reservation_id = ?1"),
            [device_scheduler_reservation_id],
            reservation_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(complete_stored_reservation)
        .transpose()
}

fn stored_reservation_by_request(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<StoredReservation>, DeviceSchedulerStoreError> {
    let reservation_id = connection
        .query_row(
            "SELECT device_scheduler_reservation_id FROM device_scheduler_reservations
             WHERE request_id = ?1",
            [request_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    match reservation_id {
        Some(reservation_id) => stored_reservation(connection, &reservation_id),
        None => Ok(None),
    }
}

fn load_reservation(
    connection: &Connection,
    device_scheduler_reservation_id: &str,
) -> Result<Option<DeviceSchedulerReservationRecord>, DeviceSchedulerStoreError> {
    Ok(
        stored_reservation(connection, device_scheduler_reservation_id)?
            .map(|stored| stored.record),
    )
}

fn validate_schema(connection: &Connection) -> Result<(), DeviceSchedulerStoreError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(device_scheduler_reservations)")
        .map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    let expected_columns: Vec<String> = DEVICE_SCHEDULER_COLUMNS
        .iter()
        .map(|column| (*column).to_owned())
        .collect();
    if columns != expected_columns {
        return Err(error(
            DeviceSchedulerStoreErrorKind::CorruptState,
            "device scheduler reservation ledger schema is incompatible",
        ));
    }
    Ok(())
}

fn command_digest(command: &impl Serialize) -> Result<String, DeviceSchedulerStoreError> {
    let encoded = serde_json::to_vec(command).map_err(|_| {
        error(
            DeviceSchedulerStoreErrorKind::Storage,
            "device scheduler command could not be encoded",
        )
    })?;
    Ok(format!("sha256:{:x}", Sha256::digest(&encoded)))
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), DeviceSchedulerStoreError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(invalid_id(label));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(invalid_id(label));
    }
    Ok(())
}

fn validate_fencing_token(value: u64) -> Result<(), DeviceSchedulerStoreError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(error(
            DeviceSchedulerStoreErrorKind::InvalidInput,
            "occupancy fencing token is outside the durable range",
        ));
    }
    Ok(())
}

fn validate_revision(value: u64) -> Result<(), DeviceSchedulerStoreError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(error(
            DeviceSchedulerStoreErrorKind::InvalidInput,
            "reservation revision is outside the durable range",
        ));
    }
    Ok(())
}

fn validate_instant(value: &Instant, label: &str) -> Result<(), DeviceSchedulerStoreError> {
    if value.0.len() != 24 || !value.0.ends_with('Z') || !value.0.starts_with("20") {
        return Err(error(
            DeviceSchedulerStoreErrorKind::InvalidInput,
            format!("{label} is not a canonical instant"),
        ));
    }
    Ok(())
}

const fn is_crockford_base32(byte: u8) -> bool {
    matches!(
        byte,
        b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
    )
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, DeviceSchedulerStoreError> {
    u64::try_from(value).map_err(|_| {
        error(
            DeviceSchedulerStoreErrorKind::CorruptState,
            format!("stored {label} is outside the durable range"),
        )
    })
}

fn sql_integer(value: u64) -> Result<i64, DeviceSchedulerStoreError> {
    i64::try_from(value).map_err(|_| {
        error(
            DeviceSchedulerStoreErrorKind::InvalidInput,
            "integer is outside the durable range",
        )
    })
}

fn map_reservation_insert_sql(sql: &rusqlite::Error) -> DeviceSchedulerStoreError {
    if matches!(
        sql,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::ConstraintViolation,
                ..
            },
            _
        )
    ) {
        return error(
            DeviceSchedulerStoreErrorKind::RequestConflict,
            "the request or reservation identity is already used",
        );
    }
    sql_error(sql)
}

fn invalid_id(label: &str) -> DeviceSchedulerStoreError {
    error(
        DeviceSchedulerStoreErrorKind::InvalidInput,
        format!("{label} is not canonical"),
    )
}

fn unknown_reservation(reservation_id: &str) -> DeviceSchedulerStoreError {
    error(
        DeviceSchedulerStoreErrorKind::UnknownReservation,
        format!("no device scheduler reservation matches {reservation_id}"),
    )
}

fn illegal_transition(
    record: &DeviceSchedulerReservationRecord,
    detail: &str,
) -> DeviceSchedulerStoreError {
    error(
        DeviceSchedulerStoreErrorKind::IllegalStateTransition,
        format!(
            "reservation {} in state {} cannot change: {detail}",
            record.device_scheduler_reservation_id,
            record.state.as_str(),
        ),
    )
}

fn cas_lost(operation: &str) -> DeviceSchedulerStoreError {
    error(
        DeviceSchedulerStoreErrorKind::RevisionConflict,
        format!("{operation} lost the compare-and-swap race"),
    )
}

fn reservation_missing_after_write() -> DeviceSchedulerStoreError {
    error(
        DeviceSchedulerStoreErrorKind::CorruptState,
        "device scheduler reservation is missing after a durable write",
    )
}

fn error(
    kind: DeviceSchedulerStoreErrorKind,
    message: impl Into<String>,
) -> DeviceSchedulerStoreError {
    DeviceSchedulerStoreError {
        kind,
        message: message.into(),
    }
}

fn sql_error(sql: &rusqlite::Error) -> DeviceSchedulerStoreError {
    error(
        DeviceSchedulerStoreErrorKind::Storage,
        format!("device scheduler ledger storage failed: {sql}"),
    )
}

fn storage_error(storage: &StorageError) -> DeviceSchedulerStoreError {
    error(
        DeviceSchedulerStoreErrorKind::Storage,
        format!("device scheduler ledger storage is unavailable: {storage}"),
    )
}
