// SPDX-License-Identifier: Apache-2.0

//! Durable `DeviceExecutionBinding` facts and the per-node execution
//! reservation capacity ledger.
//!
//! The task-execution authority chain is: one `ProductSession` owns stage
//! runs, the scheduler reserves one execution Job per stage run, and the
//! worker that executes the Job is bound — durably, before any dispatch — to
//! one `WorkerLaunchGrant` with its client node, occupancy lease, fencing
//! token, and repository binding (plan 7.8, 14, 17.2). This ledger is the
//! join point: it stores the binding of one `worker_session_id` to the
//! `clientNodeId`/`bindingId`/`leaseId` facts of its live launch grant, and
//! the companion device facts of one execution admission reservation.
//!
//! Authoritative source rule: every binding field is copied from the stored
//! `worker_launch_grants` row inside the same immediate transaction, and the
//! binding command must echo the grant exactly. A later projection that
//! guesses any field is refused with a field mismatch — bindings are written
//! from the authority, never reconstructed.
//!
//! CAS/replay rules (fixed): every mutating command carries one canonical
//! `req_` request identity. A repeated request with a byte-identical command
//! replays the stored receipt as an accepted idempotent no-op; the same
//! request identity with any different body is a conflict. Binding rows are
//! created at revision 1 and advance only through the compare-and-swap
//! `expected_revision` release transition. At most one `bound` binding may
//! exist per worker session and at most one binding per launch grant; the
//! partial unique indexes are the durable backstops.
//!
//! Capacity ledger: the occupancy claim gate and the launch grant issue gate
//! must judge capacity against the same durable numbers. `reserved` counts
//! the non-terminal (`issued` plus `consumed`) launch grants of the node —
//! the reservation-driven persistent slot count that replaces the former
//! claim-time placeholder — and `in_flight` is the device-reported running
//! count and the durable reservation count reconciled by taking the maximum,
//! so neither claim nor launch can oversell a slot the other already holds.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;

const DEVICE_EXECUTION_BINDING_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS device_execution_bindings (
    device_execution_binding_id TEXT PRIMARY KEY NOT NULL,
    worker_session_id TEXT NOT NULL,
    client_node_id TEXT NOT NULL,
    client_instance_id TEXT NOT NULL,
    holder_user_id TEXT NOT NULL,
    repository_binding_id TEXT NOT NULL,
    occupancy_lease_id TEXT NOT NULL,
    occupancy_fencing_token INTEGER NOT NULL
        CHECK (occupancy_fencing_token > 0 AND occupancy_fencing_token <= 9007199254740991),
    worker_launch_grant_id TEXT NOT NULL,
    product_session_id TEXT,
    stage_run_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('bound', 'released')),
    bound_at TEXT NOT NULL,
    released_at TEXT,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT,
    FOREIGN KEY (worker_launch_grant_id)
        REFERENCES worker_launch_grants(worker_launch_grant_id) ON DELETE RESTRICT,
    CHECK (
        (state = 'bound' AND released_at IS NULL)
        OR (state = 'released' AND released_at IS NOT NULL)
    )
);
CREATE UNIQUE INDEX IF NOT EXISTS device_execution_bindings_one_bound_per_session
    ON device_execution_bindings (worker_session_id)
    WHERE state = 'bound';
CREATE UNIQUE INDEX IF NOT EXISTS device_execution_bindings_one_per_launch_grant
    ON device_execution_bindings (worker_launch_grant_id);
CREATE INDEX IF NOT EXISTS device_execution_bindings_by_client
    ON device_execution_bindings (client_node_id, state);
CREATE TABLE IF NOT EXISTS device_execution_reservation_facts (
    job_id TEXT PRIMARY KEY NOT NULL,
    client_node_id TEXT NOT NULL,
    client_instance_id TEXT NOT NULL,
    holder_user_id TEXT NOT NULL,
    repository_binding_id TEXT NOT NULL,
    occupancy_lease_id TEXT NOT NULL,
    occupancy_fencing_token INTEGER NOT NULL
        CHECK (occupancy_fencing_token > 0 AND occupancy_fencing_token <= 9007199254740991),
    worker_launch_grant_id TEXT NOT NULL,
    worker_session_id TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    product_session_id TEXT,
    stage_run_id TEXT,
    attached_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision = 1),
    FOREIGN KEY (job_id)
        REFERENCES execution_admission_reservations(job_id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS device_execution_reservation_facts_by_session
    ON device_execution_reservation_facts (worker_session_id);
CREATE INDEX IF NOT EXISTS device_execution_reservation_facts_by_client
    ON device_execution_reservation_facts (client_node_id);
CREATE TABLE IF NOT EXISTS device_execution_binding_receipts (
    scope_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (scope_key, request_id)
);
";

/// Scope partition of the bind/release receipt namespace.
const BINDING_SCOPE_KEY: &str = "device_execution_binding";
/// Scope partition of the reservation-facts receipt namespace.
const FACTS_SCOPE_KEY: &str = "device_execution_reservation_facts";

/// Lifecycle state of one `DeviceExecutionBinding`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceExecutionBindingState {
    /// The worker session is durably bound to its launch facts.
    Bound,
    /// Terminal: the binding ended with its worker session.
    Released,
}

impl DeviceExecutionBindingState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Released => "released",
        }
    }

    fn parse(value: &str) -> Result<Self, DeviceExecutionBindingStoreError> {
        match value {
            "bound" => Ok(Self::Bound),
            "released" => Ok(Self::Released),
            _ => Err(error(
                DeviceExecutionBindingStoreErrorKind::CorruptState,
                "stored device execution binding state is invalid",
            )),
        }
    }

    /// True while the binding still names the worker session's execution
    /// authority.
    #[must_use]
    pub const fn is_bound(self) -> bool {
        matches!(self, Self::Bound)
    }
}

impl fmt::Display for DeviceExecutionBindingState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Validated bind command. Every `expected_` field must echo the stored
/// launch grant exactly; any divergence refuses the binding.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceExecutionBindingIssuance {
    pub device_execution_binding_id: String,
    pub request_id: String,
    pub worker_launch_grant_id: String,
    pub expected_client_node_id: String,
    pub expected_client_instance_id: String,
    pub expected_holder_user_id: String,
    pub expected_occupancy_lease_id: String,
    pub expected_occupancy_fencing_token: u64,
    pub expected_repository_binding_id: String,
    pub expected_worker_session_id: String,
    pub expected_product_session_id: Option<String>,
    pub expected_stage_run_id: Option<String>,
}

impl DeviceExecutionBindingIssuance {
    /// Builds one validated bind command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities or a fencing token outside the
    /// durable range before any durable write.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        device_execution_binding_id: impl Into<String>,
        request_id: impl Into<String>,
        worker_launch_grant_id: impl Into<String>,
        expected_client_node_id: impl Into<String>,
        expected_client_instance_id: impl Into<String>,
        expected_holder_user_id: impl Into<String>,
        expected_occupancy_lease_id: impl Into<String>,
        expected_occupancy_fencing_token: u64,
        expected_repository_binding_id: impl Into<String>,
        expected_worker_session_id: impl Into<String>,
        expected_product_session_id: Option<String>,
        expected_stage_run_id: Option<String>,
    ) -> Result<Self, DeviceExecutionBindingStoreError> {
        let command = Self {
            device_execution_binding_id: device_execution_binding_id.into(),
            request_id: request_id.into(),
            worker_launch_grant_id: worker_launch_grant_id.into(),
            expected_client_node_id: expected_client_node_id.into(),
            expected_client_instance_id: expected_client_instance_id.into(),
            expected_holder_user_id: expected_holder_user_id.into(),
            expected_occupancy_lease_id: expected_occupancy_lease_id.into(),
            expected_occupancy_fencing_token,
            expected_repository_binding_id: expected_repository_binding_id.into(),
            expected_worker_session_id: expected_worker_session_id.into(),
            expected_product_session_id,
            expected_stage_run_id,
        };
        validate_device_execution_binding_id(&command.device_execution_binding_id)?;
        validate_request_id(&command.request_id)?;
        validate_worker_launch_grant_id(&command.worker_launch_grant_id)?;
        validate_client_node_id(&command.expected_client_node_id)?;
        validate_client_instance_id(&command.expected_client_instance_id)?;
        validate_user_id(&command.expected_holder_user_id)?;
        validate_occupancy_lease_id(&command.expected_occupancy_lease_id)?;
        validate_fencing_token(command.expected_occupancy_fencing_token)?;
        validate_repository_binding_id(&command.expected_repository_binding_id)?;
        validate_worker_session_id(&command.expected_worker_session_id)?;
        if let Some(product) = &command.expected_product_session_id {
            validate_product_session_id(product)?;
        }
        if let Some(stage) = &command.expected_stage_run_id {
            validate_stage_run_id(stage)?;
        }
        Ok(command)
    }
}

/// Validated release command; the fixed CAS rule requires the bound row's
/// current revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceExecutionBindingRelease {
    pub worker_session_id: String,
    pub request_id: String,
    pub expected_revision: u64,
    pub released_at: Instant,
}

impl DeviceExecutionBindingRelease {
    /// Builds one validated release command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, a zero revision, or a
    /// non-canonical instant.
    pub fn try_new(
        worker_session_id: impl Into<String>,
        request_id: impl Into<String>,
        expected_revision: u64,
        released_at: Instant,
    ) -> Result<Self, DeviceExecutionBindingStoreError> {
        let command = Self {
            worker_session_id: worker_session_id.into(),
            request_id: request_id.into(),
            expected_revision,
            released_at,
        };
        validate_worker_session_id(&command.worker_session_id)?;
        validate_request_id(&command.request_id)?;
        validate_revision(command.expected_revision)?;
        validate_instant(&command.released_at, "release time")?;
        Ok(command)
    }
}

/// Validated attachment of one execution reservation to its device facts.
/// The facts themselves are read from the stored launch grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceExecutionFactsAttachment {
    pub request_id: String,
    pub job_id: String,
    pub worker_launch_grant_id: String,
}

impl DeviceExecutionFactsAttachment {
    /// Builds one validated attachment command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities.
    pub fn try_new(
        request_id: impl Into<String>,
        job_id: impl Into<String>,
        worker_launch_grant_id: impl Into<String>,
    ) -> Result<Self, DeviceExecutionBindingStoreError> {
        let command = Self {
            request_id: request_id.into(),
            job_id: job_id.into(),
            worker_launch_grant_id: worker_launch_grant_id.into(),
        };
        validate_request_id(&command.request_id)?;
        validate_job_id(&command.job_id)?;
        validate_worker_launch_grant_id(&command.worker_launch_grant_id)?;
        Ok(command)
    }
}

/// Durable `DeviceExecutionBinding` row: one worker session bound to the
/// client, occupancy, and repository identities of its live launch grant.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceExecutionBindingRecord {
    pub device_execution_binding_id: String,
    pub worker_session_id: String,
    pub client_node_id: String,
    pub client_instance_id: String,
    pub holder_user_id: String,
    pub repository_binding_id: String,
    pub occupancy_lease_id: String,
    pub occupancy_fencing_token: u64,
    pub worker_launch_grant_id: String,
    pub product_session_id: Option<String>,
    pub stage_run_id: Option<String>,
    pub state: DeviceExecutionBindingState,
    pub bound_at: Instant,
    pub released_at: Option<Instant>,
    pub revision: u64,
}

/// Companion device facts of one execution admission reservation. Every
/// field is copied verbatim from the backing launch grant, so the Job stays
/// traceable to the client node, occupancy lease, repository binding,
/// worker, and (when stamped) the `ProductSession`/`StageRun` pair.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceExecutionReservationFacts {
    pub job_id: String,
    pub client_node_id: String,
    pub client_instance_id: String,
    pub holder_user_id: String,
    pub repository_binding_id: String,
    pub occupancy_lease_id: String,
    pub occupancy_fencing_token: u64,
    pub worker_launch_grant_id: String,
    pub worker_session_id: String,
    pub worker_id: String,
    pub worker_instance_id: String,
    pub product_session_id: Option<String>,
    pub stage_run_id: Option<String>,
    pub attached_at: Instant,
}

/// Replay-safe response for the bind and release transitions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceBindingReceipt {
    pub binding: DeviceExecutionBindingRecord,
    pub replayed: bool,
}

/// Replay-safe response for the reservation-facts attachment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceFactsReceipt {
    pub facts: DeviceExecutionReservationFacts,
    pub replayed: bool,
}

/// Transactionally consistent worker-session capacity of one client node.
///
/// `reserved_worker_sessions` is the durable ledger: the non-terminal launch
/// grants of the node. `in_flight_worker_sessions` reconciles the
/// device-reported running count with the durable reservation count by
/// taking the maximum, and `free_worker_sessions` is what claim and launch
/// gates may still allocate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceExecutionCapacitySnapshot {
    pub client_node_id: String,
    pub max_worker_sessions: u64,
    pub reported_running_worker_sessions: u64,
    pub reserved_worker_sessions: u64,
    pub bound_bindings: u64,
    pub in_flight_worker_sessions: u64,
    pub free_worker_sessions: u64,
}

/// Stable device-execution-binding failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceExecutionBindingStoreErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// No launch grant matches the requested identity.
    UnknownLaunchGrant,
    /// The launch grant is not live (`issued` or `consumed`).
    LaunchGrantNotLive,
    /// An echoed binding or attachment field differs from the authority.
    FieldMismatch,
    /// The worker session or grant already carries a binding, or the binding
    /// identity is already used.
    BindingConflict,
    /// No bound binding matches the requested worker session.
    UnknownBinding,
    /// The execution Job does not exist.
    UnknownExecutionJob,
    /// The execution Job already carries device facts.
    FactsAlreadyAttached,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost a race.
    RevisionConflict,
    /// A request identity was reused with a different body.
    RequestConflict,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free device-execution-binding error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceExecutionBindingStoreError {
    kind: DeviceExecutionBindingStoreErrorKind,
    message: String,
}

impl DeviceExecutionBindingStoreError {
    #[must_use]
    pub const fn kind(&self) -> DeviceExecutionBindingStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for DeviceExecutionBindingStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DeviceExecutionBindingStoreError {}

/// Device execution binding ledger borrowing the sole product-state
/// `SQLite` authority.
pub struct DeviceExecutionBindingLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable device execution binding ledger on this same
    /// product-state database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection, an incompatible existing schema,
    /// or a missing authoritative identity ledger.
    pub fn device_execution_binding_ledger(
        &mut self,
    ) -> Result<DeviceExecutionBindingLedger<'_>, DeviceExecutionBindingStoreError> {
        DeviceExecutionBindingLedger::new(self)
    }
}

impl<'storage> DeviceExecutionBindingLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, DeviceExecutionBindingStoreError> {
        // The binding ledger consumes authoritative identity facts: client
        // nodes, worker launch grants, and execution admission reservations.
        // Opening each ensures its schema exists before the FK-enforcing
        // statements below run.
        storage.client_node_registry().map_err(dependency_error)?;
        storage
            .worker_launch_grant_ledger()
            .map_err(dependency_error)?;
        storage.execution_admission().map_err(dependency_error)?;
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(DEVICE_EXECUTION_BINDING_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Atomically binds one worker session to the client, occupancy, and
    /// repository identities of its live launch grant.
    ///
    /// Inside one immediate transaction the gate requires: the grant exists
    /// and is non-terminal (`issued` or `consumed`), every echoed field
    /// matches the grant exactly, no binding exists for the grant, and the
    /// worker session carries no other `bound` binding. On success the
    /// `bound` binding commits with its replay receipt.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or terminal grant, any field mismatch, a reused
    /// binding identity, a conflicting replay, or storage failure.
    #[allow(clippy::too_many_lines)]
    pub fn bind(
        &mut self,
        command: &DeviceExecutionBindingIssuance,
        now: &Instant,
    ) -> Result<DeviceBindingReceipt, DeviceExecutionBindingStoreError> {
        validate_instant(now, "bind time")?;
        let request_digest = command_digest(command)?;
        let transaction = self.transaction()?;
        if let Some(response_json) = stored_receipt(
            &transaction,
            BINDING_SCOPE_KEY,
            &command.request_id,
            &request_digest,
        )? {
            let mut receipt: DeviceBindingReceipt = decode_receipt(&response_json)?;
            receipt.replayed = true;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(receipt);
        }
        let grant = require_launch_grant(&transaction, &command.worker_launch_grant_id)?;
        if !matches!(grant.state.as_str(), "issued" | "consumed") {
            return Err(error(
                DeviceExecutionBindingStoreErrorKind::LaunchGrantNotLive,
                "the launch grant is no longer live for binding",
            ));
        }
        ensure_binding_matches_grant(&grant, command)?;
        let inserted = transaction
            .execute(
                "INSERT INTO device_execution_bindings
                 (device_execution_binding_id, worker_session_id, client_node_id,
                  client_instance_id, holder_user_id, repository_binding_id,
                  occupancy_lease_id, occupancy_fencing_token, worker_launch_grant_id,
                  product_session_id, stage_run_id, state, bound_at, released_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'bound', ?12, NULL, 1)",
                params![
                    command.device_execution_binding_id,
                    grant.worker_session_id,
                    grant.client_node_id,
                    grant.client_instance_id,
                    grant.holder_user_id,
                    grant.repository_binding_id,
                    grant.occupancy_lease_id,
                    sql_integer(grant.occupancy_fencing_token)?,
                    grant.worker_launch_grant_id,
                    grant.product_session_id,
                    grant.stage_run_id,
                    now.0,
                ],
            )
            .map_err(|sql| map_binding_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                DeviceExecutionBindingStoreErrorKind::Storage,
                "device execution binding insert did not store exactly one row",
            ));
        }
        let binding = load_binding(&transaction, &command.device_execution_binding_id)?
            .ok_or_else(binding_missing_after_write)?;
        let receipt = DeviceBindingReceipt {
            binding,
            replayed: false,
        };
        insert_receipt(
            &transaction,
            BINDING_SCOPE_KEY,
            &command.request_id,
            &request_digest,
            &receipt,
        )?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(receipt)
    }

    /// Releases the `bound` binding of one worker session through the fixed
    /// compare-and-swap transition (`bound -> released`).
    ///
    /// # Errors
    ///
    /// Rejects an unknown bound binding, a lost revision race, a conflicting
    /// replay, or storage failure.
    pub fn release(
        &mut self,
        command: &DeviceExecutionBindingRelease,
        now: &Instant,
    ) -> Result<DeviceBindingReceipt, DeviceExecutionBindingStoreError> {
        validate_instant(now, "release time")?;
        let request_digest = command_digest(command)?;
        let transaction = self.transaction()?;
        if let Some(response_json) = stored_receipt(
            &transaction,
            BINDING_SCOPE_KEY,
            &command.request_id,
            &request_digest,
        )? {
            let mut receipt: DeviceBindingReceipt = decode_receipt(&response_json)?;
            receipt.replayed = true;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(receipt);
        }
        let bound = require_bound_binding(&transaction, &command.worker_session_id)?;
        let updated = transaction
            .execute(
                "UPDATE device_execution_bindings
                 SET state = 'released', released_at = ?2, revision = revision + 1
                 WHERE device_execution_binding_id = ?1
                   AND revision = ?3 AND state = 'bound'",
                params![
                    bound.device_execution_binding_id,
                    now.0,
                    sql_integer(command.expected_revision)?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(cas_lost("binding release"));
        }
        let binding = load_binding(&transaction, &bound.device_execution_binding_id)?
            .ok_or_else(binding_missing_after_write)?;
        let receipt = DeviceBindingReceipt {
            binding,
            replayed: false,
        };
        insert_receipt(
            &transaction,
            BINDING_SCOPE_KEY,
            &command.request_id,
            &request_digest,
            &receipt,
        )?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(receipt)
    }

    /// Atomically attaches the device facts of one execution admission
    /// reservation.
    ///
    /// Inside one immediate transaction the gate requires: the launch grant
    /// exists and is live, the reservation exists and is non-terminal
    /// (`queued` or `running`), the reservation's user is the grant holder,
    /// and the grant's worker session carries the `bound` binding of this
    /// grant. The stored facts are copied verbatim from the grant, so a later
    /// projection reads the authority instead of guessing. Traceability to
    /// the `ProductSession`/`StageRun` pair is durable on both sides: the
    /// facts carry the grant's stamps, and the reservation scope carries the
    /// session identity it was reserved under.
    ///
    /// # Errors
    ///
    /// Rejects an unknown grant or Job, a terminal grant or reservation, any
    /// field mismatch, a missing binding, an already attached Job, a
    /// conflicting replay, or storage failure.
    #[allow(clippy::too_many_lines)]
    pub fn attach_facts(
        &mut self,
        command: &DeviceExecutionFactsAttachment,
        now: &Instant,
    ) -> Result<DeviceFactsReceipt, DeviceExecutionBindingStoreError> {
        validate_instant(now, "attachment time")?;
        let request_digest = command_digest(command)?;
        let transaction = self.transaction()?;
        if let Some(response_json) = stored_receipt(
            &transaction,
            FACTS_SCOPE_KEY,
            &command.request_id,
            &request_digest,
        )? {
            let mut receipt: DeviceFactsReceipt = decode_receipt(&response_json)?;
            receipt.replayed = true;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(receipt);
        }
        let grant = require_launch_grant(&transaction, &command.worker_launch_grant_id)?;
        if !matches!(grant.state.as_str(), "issued" | "consumed") {
            return Err(error(
                DeviceExecutionBindingStoreErrorKind::LaunchGrantNotLive,
                "the launch grant is no longer live for reservation facts",
            ));
        }
        let reservation = transaction
            .query_row(
                "SELECT state, user_id
                 FROM execution_admission_reservations WHERE job_id = ?1",
                [command.job_id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|sql| sql_error(&sql))?;
        let Some((reservation_state, reservation_user)) = reservation else {
            return Err(error(
                DeviceExecutionBindingStoreErrorKind::UnknownExecutionJob,
                "execution reservation does not exist for the device facts",
            ));
        };
        if reservation_state != "queued" && reservation_state != "running" {
            return Err(error(
                DeviceExecutionBindingStoreErrorKind::IllegalStateTransition,
                "the execution reservation is terminal for device facts",
            ));
        }
        if reservation_user != grant.holder_user_id {
            return Err(error(
                DeviceExecutionBindingStoreErrorKind::FieldMismatch,
                "the execution reservation user differs from the grant holder",
            ));
        }
        let binding = transaction
            .query_row(
                "SELECT device_execution_binding_id FROM device_execution_bindings
                 WHERE worker_session_id = ?1 AND worker_launch_grant_id = ?2
                   AND state = 'bound'",
                params![grant.worker_session_id, grant.worker_launch_grant_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|sql| sql_error(&sql))?;
        if binding.is_none() {
            return Err(error(
                DeviceExecutionBindingStoreErrorKind::UnknownBinding,
                "the grant's worker session carries no bound binding for the facts",
            ));
        }
        let facts = DeviceExecutionReservationFacts {
            job_id: command.job_id.clone(),
            client_node_id: grant.client_node_id,
            client_instance_id: grant.client_instance_id,
            holder_user_id: grant.holder_user_id,
            repository_binding_id: grant.repository_binding_id,
            occupancy_lease_id: grant.occupancy_lease_id,
            occupancy_fencing_token: grant.occupancy_fencing_token,
            worker_launch_grant_id: grant.worker_launch_grant_id,
            worker_session_id: grant.worker_session_id,
            worker_id: grant.worker_id,
            worker_instance_id: grant.worker_instance_id,
            product_session_id: grant.product_session_id,
            stage_run_id: grant.stage_run_id,
            attached_at: now.clone(),
        };
        let inserted = transaction
            .execute(
                "INSERT INTO device_execution_reservation_facts
                 (job_id, client_node_id, client_instance_id, holder_user_id,
                  repository_binding_id, occupancy_lease_id, occupancy_fencing_token,
                  worker_launch_grant_id, worker_session_id, worker_id,
                  worker_instance_id, product_session_id, stage_run_id,
                  attached_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 1)",
                params![
                    facts.job_id,
                    facts.client_node_id,
                    facts.client_instance_id,
                    facts.holder_user_id,
                    facts.repository_binding_id,
                    facts.occupancy_lease_id,
                    sql_integer(facts.occupancy_fencing_token)?,
                    facts.worker_launch_grant_id,
                    facts.worker_session_id,
                    facts.worker_id,
                    facts.worker_instance_id,
                    facts.product_session_id,
                    facts.stage_run_id,
                    facts.attached_at.0,
                ],
            )
            .map_err(|sql| map_facts_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                DeviceExecutionBindingStoreErrorKind::Storage,
                "device execution reservation facts insert did not store exactly one row",
            ));
        }
        let receipt = DeviceFactsReceipt {
            facts,
            replayed: false,
        };
        insert_receipt(
            &transaction,
            FACTS_SCOPE_KEY,
            &command.request_id,
            &request_digest,
            &receipt,
        )?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(receipt)
    }

    /// Returns the newest binding of a worker session: the `bound` binding
    /// when one exists, otherwise the most recently bound row.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical worker session identity, corrupt stored rows,
    /// or storage failure.
    pub fn snapshot(
        &self,
        worker_session_id: &str,
    ) -> Result<Option<DeviceExecutionBindingRecord>, DeviceExecutionBindingStoreError> {
        validate_worker_session_id(worker_session_id)?;
        load_session_binding(self.connection()?, worker_session_id)
    }

    /// Returns one binding by its own identity.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical binding identity, corrupt stored rows, or
    /// storage failure.
    pub fn snapshot_by_binding_id(
        &self,
        device_execution_binding_id: &str,
    ) -> Result<Option<DeviceExecutionBindingRecord>, DeviceExecutionBindingStoreError> {
        validate_device_execution_binding_id(device_execution_binding_id)?;
        load_binding(self.connection()?, device_execution_binding_id)
    }

    /// Returns the durable device facts of one execution reservation.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical Job identity, corrupt stored rows, or storage
    /// failure.
    pub fn facts(
        &self,
        job_id: &str,
    ) -> Result<Option<DeviceExecutionReservationFacts>, DeviceExecutionBindingStoreError> {
        validate_job_id(job_id)?;
        load_facts(self.connection()?, job_id)
    }

    /// Reads the durable worker-session capacity ledger of one client node.
    ///
    /// Returns `None` when the node does not exist. Both the occupancy claim
    /// gate and the launch grant issue gate judge capacity against this one
    /// durable view: `reserved_worker_sessions` counts the non-terminal
    /// launch grants, and `in_flight_worker_sessions` reconciles the
    /// device-reported running count with the durable reservations.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or storage failure.
    pub fn capacity_snapshot(
        &self,
        client_node_id: &str,
    ) -> Result<Option<DeviceExecutionCapacitySnapshot>, DeviceExecutionBindingStoreError> {
        validate_client_node_id(client_node_id)?;
        let connection = self.connection()?;
        let node = connection
            .query_row(
                "SELECT max_concurrent_worker_sessions, reported_running_worker_sessions
                 FROM client_nodes WHERE client_node_id = ?1",
                [client_node_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|sql| sql_error(&sql))?;
        let Some((max_slots, running)) = node else {
            return Ok(None);
        };
        let max_worker_sessions = from_sql_integer(max_slots, "client worker session capacity")?;
        let reported_running = from_sql_integer(running, "reported running worker sessions")?;
        let reserved_worker_sessions = self.reserved_worker_sessions_for_node(client_node_id)?;
        let bound: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM device_execution_bindings
                 WHERE client_node_id = ?1 AND state = 'bound'",
                [client_node_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|sql| sql_error(&sql))?;
        let bound_bindings = from_sql_integer(bound, "bound binding count")?;
        let in_flight_worker_sessions = reported_running.max(reserved_worker_sessions);
        let free_worker_sessions = max_worker_sessions.saturating_sub(in_flight_worker_sessions);
        Ok(Some(DeviceExecutionCapacitySnapshot {
            client_node_id: client_node_id.to_owned(),
            max_worker_sessions,
            reported_running_worker_sessions: reported_running,
            reserved_worker_sessions,
            bound_bindings,
            in_flight_worker_sessions,
            free_worker_sessions,
        }))
    }

    /// Counts the durable reservation view of one client node: its
    /// non-terminal (`issued` plus `consumed`) launch grants. This is the
    /// persistent slot count that replaces the former claim-time placeholder.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or storage failure.
    pub fn reserved_worker_sessions_for_node(
        &self,
        client_node_id: &str,
    ) -> Result<u64, DeviceExecutionBindingStoreError> {
        validate_client_node_id(client_node_id)?;
        let stored: i64 = self
            .connection()?
            .query_row(
                "SELECT COUNT(*) FROM worker_launch_grants
                 WHERE client_node_id = ?1 AND state IN ('issued', 'consumed')",
                [client_node_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|sql| sql_error(&sql))?;
        from_sql_integer(stored, "reserved worker session count")
    }

    fn transaction(
        &mut self,
    ) -> Result<rusqlite::Transaction<'_>, DeviceExecutionBindingStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&Connection, DeviceExecutionBindingStoreError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

/// Authoritative launch-grant projection the binding gate judges. Only the
/// columns the binding and attachment gates read are selected.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LaunchGrantFact {
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
    product_session_id: Option<String>,
    stage_run_id: Option<String>,
    state: String,
}

fn require_launch_grant(
    connection: &Connection,
    worker_launch_grant_id: &str,
) -> Result<LaunchGrantFact, DeviceExecutionBindingStoreError> {
    let row = connection
        .query_row(
            "SELECT worker_launch_grant_id, client_node_id, client_instance_id,
                    holder_user_id, occupancy_lease_id, occupancy_fencing_token,
                    repository_binding_id, worker_session_id, worker_id,
                    worker_instance_id, product_session_id, stage_run_id, state
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
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, String>(12)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    let Some((
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
        product_session_id,
        stage_run_id,
        state,
    )) = row
    else {
        return Err(error(
            DeviceExecutionBindingStoreErrorKind::UnknownLaunchGrant,
            "worker launch grant does not exist",
        ));
    };
    Ok(LaunchGrantFact {
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
        product_session_id,
        stage_run_id,
        state,
    })
}

/// Judges the echoed binding fields against the stored grant; any divergence
/// refuses the binding without a durable change.
fn ensure_binding_matches_grant(
    grant: &LaunchGrantFact,
    command: &DeviceExecutionBindingIssuance,
) -> Result<(), DeviceExecutionBindingStoreError> {
    let consistent = grant.worker_launch_grant_id == command.worker_launch_grant_id
        && grant.client_node_id == command.expected_client_node_id
        && grant.client_instance_id == command.expected_client_instance_id
        && grant.holder_user_id == command.expected_holder_user_id
        && grant.occupancy_lease_id == command.expected_occupancy_lease_id
        && grant.occupancy_fencing_token == command.expected_occupancy_fencing_token
        && grant.repository_binding_id == command.expected_repository_binding_id
        && grant.worker_session_id == command.expected_worker_session_id
        && grant.product_session_id == command.expected_product_session_id
        && grant.stage_run_id == command.expected_stage_run_id;
    if consistent {
        Ok(())
    } else {
        Err(error(
            DeviceExecutionBindingStoreErrorKind::FieldMismatch,
            "the binding command does not match the stored launch grant",
        ))
    }
}

#[allow(clippy::type_complexity)]
fn binding_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
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
fn complete_binding(
    row: (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
        i64,
    ),
) -> Result<DeviceExecutionBindingRecord, DeviceExecutionBindingStoreError> {
    let (
        device_execution_binding_id,
        worker_session_id,
        client_node_id,
        client_instance_id,
        holder_user_id,
        repository_binding_id,
        occupancy_lease_id,
        occupancy_fencing_token,
        worker_launch_grant_id,
        product_session_id,
        stage_run_id,
        state,
        bound_at,
        released_at,
        revision,
    ) = row;
    Ok(DeviceExecutionBindingRecord {
        device_execution_binding_id,
        worker_session_id,
        client_node_id,
        client_instance_id,
        holder_user_id,
        repository_binding_id,
        occupancy_lease_id,
        occupancy_fencing_token: from_sql_integer(
            occupancy_fencing_token,
            "occupancy fencing token",
        )?,
        worker_launch_grant_id,
        product_session_id,
        stage_run_id,
        state: DeviceExecutionBindingState::parse(&state)?,
        bound_at: parse_stored_instant(&bound_at, "bind time")?,
        released_at: released_at
            .map(|value| parse_stored_instant(&value, "release time"))
            .transpose()?,
        revision: from_sql_integer(revision, "binding revision")?,
    })
}

const BINDING_SELECT: &str = "SELECT device_execution_binding_id, worker_session_id,
        client_node_id, client_instance_id, holder_user_id, repository_binding_id,
        occupancy_lease_id, occupancy_fencing_token, worker_launch_grant_id,
        product_session_id, stage_run_id, state, bound_at, released_at, revision
 FROM device_execution_bindings";

fn load_binding(
    connection: &Connection,
    device_execution_binding_id: &str,
) -> Result<Option<DeviceExecutionBindingRecord>, DeviceExecutionBindingStoreError> {
    connection
        .query_row(
            &format!("{BINDING_SELECT} WHERE device_execution_binding_id = ?1"),
            [device_execution_binding_id],
            binding_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(complete_binding)
        .transpose()
}

fn load_session_binding(
    connection: &Connection,
    worker_session_id: &str,
) -> Result<Option<DeviceExecutionBindingRecord>, DeviceExecutionBindingStoreError> {
    connection
        .query_row(
            &format!(
                "{BINDING_SELECT} WHERE worker_session_id = ?1
                 ORDER BY (state = 'bound') DESC, bound_at DESC,
                          device_execution_binding_id DESC
                 LIMIT 1"
            ),
            [worker_session_id],
            binding_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(complete_binding)
        .transpose()
}

fn require_bound_binding(
    connection: &Connection,
    worker_session_id: &str,
) -> Result<DeviceExecutionBindingRecord, DeviceExecutionBindingStoreError> {
    load_session_binding(connection, worker_session_id)?
        .and_then(|binding| binding.state.is_bound().then_some(binding))
        .ok_or_else(|| {
            error(
                DeviceExecutionBindingStoreErrorKind::UnknownBinding,
                "worker session carries no bound device execution binding",
            )
        })
}

#[allow(clippy::type_complexity)]
fn complete_facts(
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
        String,
        Option<String>,
        Option<String>,
        String,
    ),
) -> Result<DeviceExecutionReservationFacts, DeviceExecutionBindingStoreError> {
    let (
        job_id,
        client_node_id,
        client_instance_id,
        holder_user_id,
        repository_binding_id,
        occupancy_lease_id,
        occupancy_fencing_token,
        worker_launch_grant_id,
        worker_session_id,
        worker_id,
        worker_instance_id,
        product_session_id,
        stage_run_id,
        attached_at,
    ) = row;
    Ok(DeviceExecutionReservationFacts {
        job_id,
        client_node_id,
        client_instance_id,
        holder_user_id,
        repository_binding_id,
        occupancy_lease_id,
        occupancy_fencing_token: from_sql_integer(
            occupancy_fencing_token,
            "occupancy fencing token",
        )?,
        worker_launch_grant_id,
        worker_session_id,
        worker_id,
        worker_instance_id,
        product_session_id,
        stage_run_id,
        attached_at: parse_stored_instant(&attached_at, "attachment time")?,
    })
}

#[allow(clippy::type_complexity)]
fn facts_row(
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
    String,
    Option<String>,
    Option<String>,
    String,
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
    ))
}

fn load_facts(
    connection: &Connection,
    job_id: &str,
) -> Result<Option<DeviceExecutionReservationFacts>, DeviceExecutionBindingStoreError> {
    connection
        .query_row(
            "SELECT job_id, client_node_id, client_instance_id, holder_user_id,
                    repository_binding_id, occupancy_lease_id, occupancy_fencing_token,
                    worker_launch_grant_id, worker_session_id, worker_id,
                    worker_instance_id, product_session_id, stage_run_id, attached_at
             FROM device_execution_reservation_facts WHERE job_id = ?1",
            [job_id],
            facts_row,
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(complete_facts)
        .transpose()
}

/// Replays a stored receipt when the request identity and body digest match;
/// a reused identity with a different body is a fixed conflict.
fn stored_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &str,
    request_digest: &str,
) -> Result<Option<String>, DeviceExecutionBindingStoreError> {
    let stored = connection
        .query_row(
            "SELECT request_digest, response_json FROM device_execution_binding_receipts
             WHERE scope_key = ?1 AND request_id = ?2",
            params![scope_key, request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    match stored {
        None => Ok(None),
        Some((stored_digest, response_json)) if stored_digest == request_digest => {
            Ok(Some(response_json))
        }
        Some(_) => Err(error(
            DeviceExecutionBindingStoreErrorKind::RequestConflict,
            "device execution binding request id was reused with a different body",
        )),
    }
}

fn decode_receipt<R: serde::de::DeserializeOwned>(
    response_json: &str,
) -> Result<R, DeviceExecutionBindingStoreError> {
    serde_json::from_str(response_json).map_err(|_| {
        error(
            DeviceExecutionBindingStoreErrorKind::CorruptState,
            "stored device execution binding receipt is invalid",
        )
    })
}

fn insert_receipt<R: Serialize>(
    connection: &Connection,
    scope_key: &str,
    request_id: &str,
    request_digest: &str,
    receipt: &R,
) -> Result<(), DeviceExecutionBindingStoreError> {
    let response_json = serde_json::to_string(receipt).map_err(|_| {
        error(
            DeviceExecutionBindingStoreErrorKind::Storage,
            "device execution binding receipt could not be encoded",
        )
    })?;
    connection
        .execute(
            "INSERT INTO device_execution_binding_receipts
                (scope_key, request_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![scope_key, request_id, request_digest, response_json],
        )
        .map_err(|sql| sql_error(&sql))?;
    Ok(())
}

/// Canonical request body digest: the SHA-256 of the command's canonical
/// JSON encoding.
fn command_digest(command: &impl Serialize) -> Result<String, DeviceExecutionBindingStoreError> {
    let encoded = serde_json::to_vec(command).map_err(|_| {
        error(
            DeviceExecutionBindingStoreErrorKind::Storage,
            "device execution binding command could not be encoded",
        )
    })?;
    Ok(format!("sha256:{:x}", Sha256::digest(&encoded)))
}

fn validate_schema(connection: &Connection) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_columns(
        connection,
        "device_execution_bindings",
        &[
            "device_execution_binding_id",
            "worker_session_id",
            "client_node_id",
            "client_instance_id",
            "holder_user_id",
            "repository_binding_id",
            "occupancy_lease_id",
            "occupancy_fencing_token",
            "worker_launch_grant_id",
            "product_session_id",
            "stage_run_id",
            "state",
            "bound_at",
            "released_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "device_execution_reservation_facts",
        &[
            "job_id",
            "client_node_id",
            "client_instance_id",
            "holder_user_id",
            "repository_binding_id",
            "occupancy_lease_id",
            "occupancy_fencing_token",
            "worker_launch_grant_id",
            "worker_session_id",
            "worker_id",
            "worker_instance_id",
            "product_session_id",
            "stage_run_id",
            "attached_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "device_execution_binding_receipts",
        &["scope_key", "request_id", "request_digest", "response_json"],
    )
}

fn validate_columns(
    connection: &Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), DeviceExecutionBindingStoreError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            DeviceExecutionBindingStoreErrorKind::CorruptState,
            "device execution binding ledger schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), DeviceExecutionBindingStoreError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(invalid_id(label));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(invalid_id(label));
    }
    Ok(())
}

fn invalid_id(label: &str) -> DeviceExecutionBindingStoreError {
    error(
        DeviceExecutionBindingStoreErrorKind::InvalidInput,
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

fn validate_device_execution_binding_id(
    value: &str,
) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "deb_", "device execution binding id")
}

fn validate_client_node_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "cnd_", "client node id")
}

fn validate_client_instance_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "cix_", "client instance id")
}

fn validate_user_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "usr_", "user id")
}

fn validate_occupancy_lease_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "ocl_", "occupancy lease id")
}

fn validate_repository_binding_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "rbd_", "repository binding id")
}

fn validate_worker_session_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "ws_", "worker session id")
}

fn validate_worker_launch_grant_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "wlg_", "worker launch grant id")
}

fn validate_product_session_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "ps_", "product session id")
}

fn validate_stage_run_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "run_", "stage run id")
}

fn validate_request_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "req_", "request id")
}

fn validate_job_id(value: &str) -> Result<(), DeviceExecutionBindingStoreError> {
    validate_crockford_id(value, "job_", "execution job id")
}

fn validate_revision(value: u64) -> Result<(), DeviceExecutionBindingStoreError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(error(
            DeviceExecutionBindingStoreErrorKind::InvalidInput,
            "revision is outside the durable range",
        ));
    }
    Ok(())
}

fn validate_fencing_token(value: u64) -> Result<(), DeviceExecutionBindingStoreError> {
    if value == 0 || value > MAX_SAFE_INTEGER {
        return Err(error(
            DeviceExecutionBindingStoreErrorKind::InvalidInput,
            "fencing token is outside the durable range",
        ));
    }
    Ok(())
}

/// Validates the canonical `domain.Instant` shape (`YYYY-MM-DDTHH:MM:SS.sssZ`).
fn validate_instant(value: &Instant, label: &str) -> Result<(), DeviceExecutionBindingStoreError> {
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
            DeviceExecutionBindingStoreErrorKind::InvalidInput,
            format!("{label} instant is not canonical"),
        ))
    }
}

fn parse_stored_instant(
    value: &str,
    label: &str,
) -> Result<Instant, DeviceExecutionBindingStoreError> {
    let instant = Instant(value.to_owned());
    validate_instant(&instant, label).map(|()| instant)
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, DeviceExecutionBindingStoreError> {
    u64::try_from(value).map_err(|_| {
        error(
            DeviceExecutionBindingStoreErrorKind::CorruptState,
            format!("stored {label} is outside the supported range"),
        )
    })
}

fn sql_integer(value: u64) -> Result<i64, DeviceExecutionBindingStoreError> {
    i64::try_from(value).map_err(|_| {
        error(
            DeviceExecutionBindingStoreErrorKind::InvalidInput,
            "command integer is outside the durable range",
        )
    })
}

fn error(
    kind: DeviceExecutionBindingStoreErrorKind,
    message: impl Into<String>,
) -> DeviceExecutionBindingStoreError {
    DeviceExecutionBindingStoreError {
        kind,
        message: message.into(),
    }
}

fn sql_error(sql: &rusqlite::Error) -> DeviceExecutionBindingStoreError {
    error(
        DeviceExecutionBindingStoreErrorKind::Storage,
        format!("device execution binding storage failed: {sql}"),
    )
}

fn storage_error(storage: &StorageError) -> DeviceExecutionBindingStoreError {
    error(
        DeviceExecutionBindingStoreErrorKind::Storage,
        format!("device execution binding storage failed: {storage}"),
    )
}

fn dependency_error(source: impl fmt::Display) -> DeviceExecutionBindingStoreError {
    error(
        DeviceExecutionBindingStoreErrorKind::Storage,
        format!("device execution binding authority ledger failed to open: {source}"),
    )
}

fn map_binding_insert_sql(sql: &rusqlite::Error) -> DeviceExecutionBindingStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            // The realistic unique violations are the one-bound-binding-per-
            // session partial index and the one-binding-per-grant index; a
            // binding id reuse shares the extended code family and fails
            // closed as a conflict too.
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                DeviceExecutionBindingStoreErrorKind::BindingConflict,
                "the worker session or launch grant already carries a device execution binding",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                DeviceExecutionBindingStoreErrorKind::BindingConflict,
                "device execution binding id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => error(
                DeviceExecutionBindingStoreErrorKind::CorruptState,
                "device execution binding references a missing authoritative row",
            ),
            _ => error(
                DeviceExecutionBindingStoreErrorKind::InvalidInput,
                "device execution binding violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn map_facts_insert_sql(sql: &rusqlite::Error) -> DeviceExecutionBindingStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
            | rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                DeviceExecutionBindingStoreErrorKind::FactsAlreadyAttached,
                "the execution job already carries device execution reservation facts",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => error(
                DeviceExecutionBindingStoreErrorKind::UnknownExecutionJob,
                "execution reservation does not exist for the device facts",
            ),
            _ => error(
                DeviceExecutionBindingStoreErrorKind::InvalidInput,
                "device execution reservation facts violate a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn binding_missing_after_write() -> DeviceExecutionBindingStoreError {
    error(
        DeviceExecutionBindingStoreErrorKind::CorruptState,
        "device execution binding row is missing after the write",
    )
}

fn cas_lost(action: &str) -> DeviceExecutionBindingStoreError {
    error(
        DeviceExecutionBindingStoreErrorKind::RevisionConflict,
        format!("the {action} compare-and-swap guard lost its race"),
    )
}
