// SPDX-License-Identifier: Apache-2.0

//! Durable Server-side `ClientNode` registry and Client exchange cursors.
//!
//! This module stores the persisted Control Plane projection of device-reported
//! Client facts (ADR-0030): identity, presence, capacity, heartbeat, and the
//! per-client bidirectional `ClientControlPort` exchange acknowledgement cursors
//! that must survive a Server restart. Presence states and their legal
//! transitions follow the frozen state machine in
//! `docs/contracts/client-control-state-machines.md`; every mutation uses
//! optimistic `expectedRevision` compare-and-swap on the node revision.

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;
const MAX_SESSION_SLOTS: u32 = 1024;
const MAX_DISPLAY_NAME_CHARS: usize = 200;
const MAX_CLIENT_VERSION_BYTES: usize = 64;

const CLIENT_REGISTRY_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS client_nodes (
    client_node_id TEXT PRIMARY KEY NOT NULL,
    public_client_id TEXT UNIQUE NOT NULL,
    display_name TEXT NOT NULL,
    platform TEXT NOT NULL CHECK (platform IN (
        'aarch64-apple-darwin',
        'x86_64-apple-darwin',
        'aarch64-unknown-linux-gnu',
        'x86_64-unknown-linux-gnu')),
    architecture TEXT NOT NULL CHECK (architecture IN ('aarch64', 'x86_64')),
    client_version TEXT NOT NULL,
    device_credential_digest TEXT,
    current_instance_id TEXT,
    presence_state TEXT NOT NULL DEFAULT 'pending_enrollment'
        CHECK (presence_state IN (
            'pending_enrollment', 'online', 'degraded', 'offline', 'locked', 'revoked')),
    accepting_connections INTEGER NOT NULL DEFAULT 1 CHECK (accepting_connections IN (0, 1)),
    lock_state TEXT NOT NULL DEFAULT 'unlocked' CHECK (lock_state IN ('unlocked', 'locked')),
    max_concurrent_worker_sessions INTEGER NOT NULL DEFAULT 0
        CHECK (max_concurrent_worker_sessions >= 0 AND max_concurrent_worker_sessions <= 1024),
    reported_running_worker_sessions INTEGER NOT NULL DEFAULT 0
        CHECK (reported_running_worker_sessions >= 0 AND reported_running_worker_sessions <= 1024),
    last_heartbeat_at TEXT,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0 AND revision <= 9007199254740991)
);
CREATE TABLE IF NOT EXISTS client_exchange_cursors (
    client_node_id TEXT PRIMARY KEY NOT NULL,
    client_to_server_ack_sequence INTEGER NOT NULL DEFAULT 0
        CHECK (client_to_server_ack_sequence >= 0),
    server_to_client_ack_sequence INTEGER NOT NULL DEFAULT 0
        CHECK (server_to_client_ack_sequence >= 0),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT
);
";

/// Machine-level presence of one `ClientNode` (plan 4.1, contract 1).
///
/// It belongs to no browser session. `Revoked` is the only terminal state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientPresenceState {
    /// Registered but not yet accepted (`client.enroll` recorded).
    PendingEnrollment,
    /// Reachable and exchanging within the heartbeat window.
    Online,
    /// Reachable but local Worker/occupancy reconciliation is unfinished.
    Degraded,
    /// Heartbeat/exchange did not arrive within the timeout window.
    Offline,
    /// Locked by a local operator or `client.client_lock`.
    Locked,
    /// Terminal: the device or its credential was revoked.
    Revoked,
}

impl ClientPresenceState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PendingEnrollment => "pending_enrollment",
            Self::Online => "online",
            Self::Degraded => "degraded",
            Self::Offline => "offline",
            Self::Locked => "locked",
            Self::Revoked => "revoked",
        }
    }

    fn parse(value: &str) -> Result<Self, ClientRegistryError> {
        match value {
            "pending_enrollment" => Ok(Self::PendingEnrollment),
            "online" => Ok(Self::Online),
            "degraded" => Ok(Self::Degraded),
            "offline" => Ok(Self::Offline),
            "locked" => Ok(Self::Locked),
            "revoked" => Ok(Self::Revoked),
            _ => Err(error(
                ClientRegistryErrorKind::CorruptState,
                "stored client presence state is invalid",
            )),
        }
    }
}

impl fmt::Display for ClientPresenceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Machine-level lock switch on one `ClientNode` (plan 7.2, 12.1).
///
/// It is an independent boolean projection, not a presence state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientLockState {
    /// The node accepts new connections and occupancy.
    Unlocked,
    /// The node is locked and rejects new work.
    Locked,
}

impl ClientLockState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unlocked => "unlocked",
            Self::Locked => "locked",
        }
    }

    fn parse(value: &str) -> Result<Self, ClientRegistryError> {
        match value {
            "unlocked" => Ok(Self::Unlocked),
            "locked" => Ok(Self::Locked),
            _ => Err(error(
                ClientRegistryErrorKind::CorruptState,
                "stored client lock state is invalid",
            )),
        }
    }
}

impl fmt::Display for ClientLockState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Device-reported `ClientNode` registration facts (plan 7.2).
///
/// Device Client owns these facts; the Control Plane persists the projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientNodeRegistration {
    client_node_id: String,
    public_client_id: String,
    display_name: String,
    platform: String,
    architecture: String,
    client_version: String,
    device_credential_digest: Option<String>,
    current_instance_id: Option<String>,
    max_concurrent_worker_sessions: u32,
}

impl ClientNodeRegistration {
    /// Builds one validated registration command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, out-of-range fields, and unsupported
    /// platform or architecture values before any durable write.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        client_node_id: impl Into<String>,
        public_client_id: impl Into<String>,
        display_name: impl Into<String>,
        platform: impl Into<String>,
        architecture: impl Into<String>,
        client_version: impl Into<String>,
        device_credential_digest: Option<String>,
        current_instance_id: Option<String>,
        max_concurrent_worker_sessions: u32,
    ) -> Result<Self, ClientRegistryError> {
        let registration = Self {
            client_node_id: client_node_id.into(),
            public_client_id: public_client_id.into(),
            display_name: display_name.into(),
            platform: platform.into(),
            architecture: architecture.into(),
            client_version: client_version.into(),
            device_credential_digest,
            current_instance_id,
            max_concurrent_worker_sessions,
        };
        registration.validate()?;
        Ok(registration)
    }

    fn validate(&self) -> Result<(), ClientRegistryError> {
        validate_client_node_id(&self.client_node_id)?;
        validate_public_client_id(&self.public_client_id)?;
        if self.display_name.is_empty()
            || self.display_name.chars().count() > MAX_DISPLAY_NAME_CHARS
        {
            return Err(error(
                ClientRegistryErrorKind::InvalidInput,
                "client display name must contain 1 to 200 characters",
            ));
        }
        if !matches!(
            self.platform.as_str(),
            "aarch64-apple-darwin"
                | "x86_64-apple-darwin"
                | "aarch64-unknown-linux-gnu"
                | "x86_64-unknown-linux-gnu"
        ) {
            return Err(error(
                ClientRegistryErrorKind::InvalidInput,
                "client platform is not a supported release target",
            ));
        }
        if !matches!(self.architecture.as_str(), "aarch64" | "x86_64") {
            return Err(error(
                ClientRegistryErrorKind::InvalidInput,
                "client architecture is not supported",
            ));
        }
        validate_client_version(&self.client_version)?;
        if let Some(digest) = &self.device_credential_digest {
            validate_sha256_digest(digest)?;
        }
        if let Some(instance_id) = &self.current_instance_id {
            validate_client_instance_id(instance_id)?;
        }
        if self.max_concurrent_worker_sessions > MAX_SESSION_SLOTS {
            return Err(error(
                ClientRegistryErrorKind::InvalidInput,
                "client concurrent worker session capacity exceeds the schema maximum",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn public_client_id(&self) -> &str {
        &self.public_client_id
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    #[must_use]
    pub fn platform(&self) -> &str {
        &self.platform
    }

    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    #[must_use]
    pub fn client_version(&self) -> &str {
        &self.client_version
    }

    #[must_use]
    pub fn device_credential_digest(&self) -> Option<&str> {
        self.device_credential_digest.as_deref()
    }

    #[must_use]
    pub fn current_instance_id(&self) -> Option<&str> {
        self.current_instance_id.as_deref()
    }

    #[must_use]
    pub const fn max_concurrent_worker_sessions(&self) -> u32 {
        self.max_concurrent_worker_sessions
    }
}

/// Durable `ClientNode` projection row (plan 7.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientNodeRecord {
    /// Stable Server-side node identifier.
    pub client_node_id: String,
    /// Stable public device number, not a secret.
    pub public_client_id: String,
    /// Human-readable device name.
    pub display_name: String,
    /// Operating system platform release target.
    pub platform: String,
    /// CPU architecture.
    pub architecture: String,
    /// Device client software version.
    pub client_version: String,
    /// Digest of the device credential; the credential itself never persists.
    pub device_credential_digest: Option<String>,
    /// Current Device Client process instance identifier.
    pub current_instance_id: Option<String>,
    /// Machine-level presence state.
    pub presence_state: ClientPresenceState,
    /// Whether the node accepts new connections.
    pub accepting_connections: bool,
    /// Machine-level lock switch.
    pub lock_state: ClientLockState,
    /// Device-reported concurrent Worker session capacity.
    pub max_concurrent_worker_sessions: u32,
    /// Device-reported currently running Worker sessions.
    pub reported_running_worker_sessions: u32,
    /// Last accepted heartbeat instant, if any.
    pub last_heartbeat_at: Option<Instant>,
    /// Instant the registry record was created.
    pub created_at: Instant,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Per-`ClientNode` bidirectional `ClientControlPort` exchange cursors (plan 9.2).
///
/// Both directions are keyed by `clientNodeId` so an exchange can resume after
/// a Server restart or network partition without replaying settled frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientExchangeCursors {
    /// Highest acknowledged Client-to-Server sequence.
    pub client_to_server_ack_sequence: u64,
    /// Highest acknowledged Server-to-Client sequence.
    pub server_to_client_ack_sequence: u64,
}

/// Result of a registry registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientNodeRegistrationReceipt {
    /// Exact durable projection after the registration.
    pub record: ClientNodeRecord,
    /// True when a new `pending_enrollment` identity was created; false when an
    /// existing identity had its device-reported projection refreshed.
    pub enrolled: bool,
}

/// Stable registry failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRegistryErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// The identity binding conflicts with durable facts, or the identity is
    /// terminal (`revoked`) and cannot be reused.
    IdentityConflict,
    /// The supplied `expectedRevision` no longer matches the durable revision.
    RevisionConflict,
    /// The requested presence change is not a legal state machine transition.
    PresenceTransition,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free `ClientNode` registry error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientRegistryError {
    kind: ClientRegistryErrorKind,
    message: String,
}

impl ClientRegistryError {
    #[must_use]
    pub const fn kind(&self) -> ClientRegistryErrorKind {
        self.kind
    }
}

impl fmt::Display for ClientRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientRegistryError {}

/// `ClientNode` registry borrowing the sole product-state `SQLite` authority.
pub struct ClientNodeRegistry<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable `ClientNode` registry on this same product-state database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
    pub fn client_node_registry(&mut self) -> Result<ClientNodeRegistry<'_>, ClientRegistryError> {
        ClientNodeRegistry::new(self)
    }
}

impl<'storage> ClientNodeRegistry<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, ClientRegistryError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(CLIENT_REGISTRY_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Registers a Device Client identity or refreshes its device-reported
    /// projection (plan 11.4 `client.enroll`).
    ///
    /// A first registration creates the node in `pending_enrollment` with a
    /// zeroed exchange cursor pair. A re-registration of the same
    /// `clientNodeId`/`publicClientId` pair refreshes the device-reported facts
    /// under `expectedRevision` compare-and-swap and leaves presence, lock, and
    /// accepting-connections facts untouched.
    ///
    /// # Errors
    ///
    /// Rejects a conflicting `publicClientId` binding, a `revoked` identity
    /// reuse, a stale `expectedRevision`, or storage failure.
    pub fn register(
        &mut self,
        registration: &ClientNodeRegistration,
        expected_revision: u64,
        now: &Instant,
    ) -> Result<ClientNodeRegistrationReceipt, ClientRegistryError> {
        registration.validate()?;
        validate_revision(expected_revision)?;
        validate_instant(now, "registration time")?;
        let transaction = self.transaction()?;
        let existing = load_client_node(&transaction, registration.client_node_id())?;
        let receipt = match existing {
            None => {
                require_public_client_id_free(&transaction, registration.public_client_id())?;
                insert_enrolled_client_node(&transaction, registration, now)?;
                registration_receipt(&transaction, registration, true, "insert")?
            }
            Some(record) => {
                ensure_registration_replay(&record, registration, expected_revision)?;
                refresh_client_node_projection(&transaction, registration, &record)?;
                registration_receipt(&transaction, registration, false, "update")?
            }
        };
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(receipt)
    }

    /// Returns one durable `ClientNode` projection.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored rows or storage failure.
    pub fn snapshot(
        &self,
        client_node_id: &str,
    ) -> Result<Option<ClientNodeRecord>, ClientRegistryError> {
        validate_client_node_id(client_node_id)?;
        load_client_node(self.connection()?, client_node_id)
    }

    /// Applies one presence state transition under `expectedRevision` CAS.
    ///
    /// Only transitions in the frozen state machine are accepted; requesting
    /// the current state is an accepted idempotent replay that leaves the
    /// revision untouched.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, a stale `expectedRevision`, an illegal
    /// transition out of the current state, or storage failure.
    pub fn update_presence(
        &mut self,
        client_node_id: &str,
        target: ClientPresenceState,
        expected_revision: u64,
    ) -> Result<ClientNodeRecord, ClientRegistryError> {
        validate_client_node_id(client_node_id)?;
        validate_revision(expected_revision)?;
        let transaction = self.transaction()?;
        let record =
            load_client_node(&transaction, client_node_id)?.ok_or_else(unknown_client_node)?;
        ensure_revision(&record, expected_revision)?;
        if record.presence_state == target {
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(record);
        }
        if !presence_transition_allowed(record.presence_state, target) {
            return Err(error(
                ClientRegistryErrorKind::PresenceTransition,
                format!(
                    "presence transition {} -> {} is not legal",
                    record.presence_state, target
                ),
            ));
        }
        let updated = transaction
            .execute(
                "UPDATE client_nodes
                 SET presence_state = ?2, revision = revision + 1
                 WHERE client_node_id = ?1 AND revision = ?3",
                params![
                    client_node_id,
                    target.as_str(),
                    sql_integer(record.revision)?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(error(
                ClientRegistryErrorKind::RevisionConflict,
                "client node revision changed during presence update",
            ));
        }
        let updated_record =
            load_client_node(&transaction, client_node_id)?.ok_or_else(unknown_client_node)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated_record)
    }

    /// Records one accepted Device Client heartbeat (plan 9.3
    /// `client.heartbeat`).
    ///
    /// The heartbeat refreshes `lastHeartbeatAt` and the reported running
    /// Worker session count. A heartbeat from `offline` reconnects the device
    /// (`offline -> online`); `pending_enrollment` and `revoked` reject
    /// heartbeats because enrollment acceptance has not happened or the
    /// identity is terminal.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, a stale `expectedRevision`, a
    /// heartbeat outside a legal state, or storage failure.
    pub fn heartbeat(
        &mut self,
        client_node_id: &str,
        reported_running_worker_sessions: u32,
        now: &Instant,
        expected_revision: u64,
    ) -> Result<ClientNodeRecord, ClientRegistryError> {
        validate_client_node_id(client_node_id)?;
        validate_instant(now, "heartbeat time")?;
        validate_revision(expected_revision)?;
        if reported_running_worker_sessions > MAX_SESSION_SLOTS {
            return Err(error(
                ClientRegistryErrorKind::InvalidInput,
                "reported running worker sessions exceed the schema maximum",
            ));
        }
        let transaction = self.transaction()?;
        let record =
            load_client_node(&transaction, client_node_id)?.ok_or_else(unknown_client_node)?;
        ensure_revision(&record, expected_revision)?;
        let reconnect = match record.presence_state {
            ClientPresenceState::Online
            | ClientPresenceState::Degraded
            | ClientPresenceState::Locked => false,
            ClientPresenceState::Offline => true,
            ClientPresenceState::PendingEnrollment | ClientPresenceState::Revoked => {
                return Err(error(
                    ClientRegistryErrorKind::PresenceTransition,
                    format!(
                        "client heartbeat is not accepted while presence is {}",
                        record.presence_state
                    ),
                ));
            }
        };
        let updated = if reconnect {
            transaction
                .execute(
                    "UPDATE client_nodes
                     SET presence_state = 'online', reported_running_worker_sessions = ?2,
                         last_heartbeat_at = ?3, revision = revision + 1
                     WHERE client_node_id = ?1 AND revision = ?4",
                    params![
                        client_node_id,
                        sql_integer(u64::from(reported_running_worker_sessions))?,
                        now.0,
                        sql_integer(record.revision)?,
                    ],
                )
                .map_err(|sql| sql_error(&sql))?
        } else {
            transaction
                .execute(
                    "UPDATE client_nodes
                     SET reported_running_worker_sessions = ?2, last_heartbeat_at = ?3,
                         revision = revision + 1
                     WHERE client_node_id = ?1 AND revision = ?4",
                    params![
                        client_node_id,
                        sql_integer(u64::from(reported_running_worker_sessions))?,
                        now.0,
                        sql_integer(record.revision)?,
                    ],
                )
                .map_err(|sql| sql_error(&sql))?
        };
        if updated != 1 {
            return Err(error(
                ClientRegistryErrorKind::RevisionConflict,
                "client node revision changed during heartbeat",
            ));
        }
        let updated_record =
            load_client_node(&transaction, client_node_id)?.ok_or_else(unknown_client_node)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(updated_record)
    }

    /// Projects unreachable `online` and `degraded` devices to `offline`
    /// (contract 1 heartbeat timeout judgement).
    ///
    /// The caller owns the timeout policy: every client whose last accepted
    /// heartbeat is at or before `cutoff` is swept. `locked` and
    /// `pending_enrollment` devices are never swept because their offline
    /// projection is not defined by the frozen state machine.
    ///
    /// # Errors
    ///
    /// Rejects an invalid cutoff or storage failure.
    pub fn sweep_offline(&mut self, cutoff: &Instant) -> Result<Vec<String>, ClientRegistryError> {
        validate_instant(cutoff, "offline cutoff")?;
        let transaction = self.transaction()?;
        let mut statement = transaction
            .prepare(
                "SELECT client_node_id, revision FROM client_nodes
                 WHERE presence_state IN ('online', 'degraded')
                   AND last_heartbeat_at IS NOT NULL AND last_heartbeat_at <= ?1",
            )
            .map_err(|sql| sql_error(&sql))?;
        let expired = statement
            .query_map([cutoff.0.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        drop(statement);
        let mut swept = Vec::with_capacity(expired.len());
        for (client_node_id, revision) in expired {
            let revision = from_sql_integer(revision, "client node revision")?;
            let updated = transaction
                .execute(
                    "UPDATE client_nodes
                     SET presence_state = 'offline', revision = revision + 1
                     WHERE client_node_id = ?1 AND revision = ?2
                       AND presence_state IN ('online', 'degraded')",
                    params![client_node_id, sql_integer(revision)?],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated == 1 {
                swept.push(client_node_id);
            }
        }
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(swept)
    }

    /// Returns the durable exchange cursors for one client node.
    ///
    /// # Errors
    ///
    /// Rejects an invalid identity, corrupt rows, or storage failure.
    pub fn exchange_cursors(
        &self,
        client_node_id: &str,
    ) -> Result<Option<ClientExchangeCursors>, ClientRegistryError> {
        validate_client_node_id(client_node_id)?;
        load_exchange_cursors(self.connection()?, client_node_id)
    }

    /// Advances the per-client bidirectional exchange acknowledgement cursors
    /// (plan 9.2 sequence/acknowledgement, 18.2 restart recovery).
    ///
    /// Each direction is monotonic: an acknowledgement at or below the stored
    /// sequence is an accepted replay no-op, never a regression.
    ///
    /// # Errors
    ///
    /// Rejects an unknown client node, an out-of-range sequence, or storage
    /// failure.
    pub fn advance_exchange_cursors(
        &mut self,
        client_node_id: &str,
        client_to_server_ack_sequence: u64,
        server_to_client_ack_sequence: u64,
    ) -> Result<ClientExchangeCursors, ClientRegistryError> {
        validate_client_node_id(client_node_id)?;
        validate_sequence(client_to_server_ack_sequence, "client-to-server ack")?;
        validate_sequence(server_to_client_ack_sequence, "server-to-client ack")?;
        let transaction = self.transaction()?;
        let updated = transaction
            .execute(
                "UPDATE client_exchange_cursors
                 SET client_to_server_ack_sequence =
                         MAX(client_to_server_ack_sequence, ?2),
                     server_to_client_ack_sequence =
                         MAX(server_to_client_ack_sequence, ?3)
                 WHERE client_node_id = ?1",
                params![
                    client_node_id,
                    sql_integer(client_to_server_ack_sequence)?,
                    sql_integer(server_to_client_ack_sequence)?,
                ],
            )
            .map_err(|sql| sql_error(&sql))?;
        if updated != 1 {
            return Err(unknown_client_node());
        }
        let cursors =
            load_exchange_cursors(&transaction, client_node_id)?.ok_or_else(unknown_client_node)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(cursors)
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, ClientRegistryError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&rusqlite::Connection, ClientRegistryError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

/// Legal presence transitions of the frozen state machine (contract 1).
const fn presence_transition_allowed(
    current: ClientPresenceState,
    target: ClientPresenceState,
) -> bool {
    use ClientPresenceState::Degraded;
    use ClientPresenceState::Locked;
    use ClientPresenceState::Offline;
    use ClientPresenceState::Online;
    use ClientPresenceState::PendingEnrollment;
    use ClientPresenceState::Revoked;
    matches!(
        (current, target),
        (PendingEnrollment, Online | Revoked)
            | (Online, Offline | Degraded | Locked | Revoked)
            | (Degraded, Online | Offline)
            | (Offline, Online | Degraded | Revoked)
            | (Locked, Online)
    )
}

fn require_public_client_id_free(
    connection: &rusqlite::Connection,
    public_client_id: &str,
) -> Result<(), ClientRegistryError> {
    let taken = connection
        .query_row(
            "SELECT client_node_id FROM client_nodes WHERE public_client_id = ?1",
            [public_client_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    if taken.is_some() {
        return Err(error(
            ClientRegistryErrorKind::IdentityConflict,
            "public client id is already bound to another client node",
        ));
    }
    Ok(())
}

fn insert_enrolled_client_node(
    transaction: &Transaction<'_>,
    registration: &ClientNodeRegistration,
    now: &Instant,
) -> Result<(), ClientRegistryError> {
    transaction
        .execute(
            "INSERT INTO client_nodes
             (client_node_id, public_client_id, display_name, platform, architecture,
              client_version, device_credential_digest, current_instance_id,
              presence_state, accepting_connections, lock_state,
              max_concurrent_worker_sessions, reported_running_worker_sessions,
              last_heartbeat_at, created_at, revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending_enrollment', 1,
                     'unlocked', ?9, 0, NULL, ?10, 1)",
            params![
                registration.client_node_id(),
                registration.public_client_id(),
                registration.display_name(),
                registration.platform(),
                registration.architecture(),
                registration.client_version(),
                registration.device_credential_digest(),
                registration.current_instance_id(),
                sql_integer(u64::from(registration.max_concurrent_worker_sessions()))?,
                now.0,
            ],
        )
        .map_err(|sql| sql_error(&sql))?;
    transaction
        .execute(
            "INSERT INTO client_exchange_cursors
             (client_node_id, client_to_server_ack_sequence, server_to_client_ack_sequence)
             VALUES (?1, 0, 0)",
            [registration.client_node_id()],
        )
        .map_err(|sql| sql_error(&sql))?;
    Ok(())
}

fn ensure_registration_replay(
    record: &ClientNodeRecord,
    registration: &ClientNodeRegistration,
    expected_revision: u64,
) -> Result<(), ClientRegistryError> {
    if record.public_client_id != registration.public_client_id() {
        return Err(error(
            ClientRegistryErrorKind::IdentityConflict,
            "client node id is already bound to another public client id",
        ));
    }
    if record.presence_state == ClientPresenceState::Revoked {
        return Err(error(
            ClientRegistryErrorKind::IdentityConflict,
            "revoked client node identity cannot re-enroll",
        ));
    }
    if record.revision != expected_revision {
        return Err(error(
            ClientRegistryErrorKind::RevisionConflict,
            "client node revision changed during registration",
        ));
    }
    Ok(())
}

fn refresh_client_node_projection(
    transaction: &Transaction<'_>,
    registration: &ClientNodeRegistration,
    record: &ClientNodeRecord,
) -> Result<(), ClientRegistryError> {
    let updated = transaction
        .execute(
            "UPDATE client_nodes
             SET display_name = ?2, platform = ?3, architecture = ?4,
                 client_version = ?5, device_credential_digest = ?6,
                 current_instance_id = ?7,
                 max_concurrent_worker_sessions = ?8,
                 revision = revision + 1
             WHERE client_node_id = ?1 AND revision = ?9",
            params![
                registration.client_node_id(),
                registration.display_name(),
                registration.platform(),
                registration.architecture(),
                registration.client_version(),
                registration.device_credential_digest(),
                registration.current_instance_id(),
                sql_integer(u64::from(registration.max_concurrent_worker_sessions()))?,
                sql_integer(record.revision)?,
            ],
        )
        .map_err(|sql| sql_error(&sql))?;
    if updated != 1 {
        return Err(error(
            ClientRegistryErrorKind::RevisionConflict,
            "client node revision changed during registration",
        ));
    }
    Ok(())
}

fn registration_receipt(
    transaction: &Transaction<'_>,
    registration: &ClientNodeRegistration,
    enrolled: bool,
    phase: &str,
) -> Result<ClientNodeRegistrationReceipt, ClientRegistryError> {
    let record =
        load_client_node(transaction, registration.client_node_id())?.ok_or_else(|| {
            error(
                ClientRegistryErrorKind::CorruptState,
                format!("registered client node row is missing after {phase}"),
            )
        })?;
    Ok(ClientNodeRegistrationReceipt { record, enrolled })
}

fn load_client_node(
    connection: &rusqlite::Connection,
    client_node_id: &str,
) -> Result<Option<ClientNodeRecord>, ClientRegistryError> {
    connection
        .query_row(
            "SELECT client_node_id, public_client_id, display_name, platform, architecture,
                    client_version, device_credential_digest, current_instance_id,
                    presence_state, accepting_connections, lock_state,
                    max_concurrent_worker_sessions, reported_running_worker_sessions,
                    last_heartbeat_at, created_at, revision
             FROM client_nodes WHERE client_node_id = ?1",
            [client_node_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, i64>(15)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(client_node_record_from_row)
        .transpose()
}

#[allow(clippy::type_complexity)]
fn client_node_record_from_row(
    row: (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        i64,
        String,
        i64,
        i64,
        Option<String>,
        String,
        i64,
    ),
) -> Result<ClientNodeRecord, ClientRegistryError> {
    let (
        client_node_id,
        public_client_id,
        display_name,
        platform,
        architecture,
        client_version,
        device_credential_digest,
        current_instance_id,
        presence_state,
        accepting_connections,
        lock_state,
        max_concurrent_worker_sessions,
        reported_running_worker_sessions,
        last_heartbeat_at,
        created_at,
        revision,
    ) = row;
    let presence_state = ClientPresenceState::parse(&presence_state)?;
    let lock_state = ClientLockState::parse(&lock_state)?;
    let accepting_connections = match accepting_connections {
        0 => false,
        1 => true,
        _ => {
            return Err(error(
                ClientRegistryErrorKind::CorruptState,
                "stored client accepting connections flag is invalid",
            ));
        }
    };
    let last_heartbeat_at = last_heartbeat_at
        .map(|value| parse_stored_instant(&value, "last heartbeat"))
        .transpose()?;
    let created_at = parse_stored_instant(&created_at, "created at")?;
    Ok(ClientNodeRecord {
        client_node_id,
        public_client_id,
        display_name,
        platform,
        architecture,
        client_version,
        device_credential_digest,
        current_instance_id,
        presence_state,
        accepting_connections,
        lock_state,
        max_concurrent_worker_sessions: stored_u32(
            max_concurrent_worker_sessions,
            "client worker session capacity",
        )?,
        reported_running_worker_sessions: stored_u32(
            reported_running_worker_sessions,
            "reported running worker sessions",
        )?,
        last_heartbeat_at,
        created_at,
        revision: from_sql_integer(revision, "client node revision")?,
    })
}

fn stored_u32(value: i64, label: &str) -> Result<u32, ClientRegistryError> {
    let value = from_sql_integer(value, label)?;
    u32::try_from(value).map_err(|_| {
        error(
            ClientRegistryErrorKind::CorruptState,
            format!("stored {label} exceeds the u32 range"),
        )
    })
}

fn load_exchange_cursors(
    connection: &rusqlite::Connection,
    client_node_id: &str,
) -> Result<Option<ClientExchangeCursors>, ClientRegistryError> {
    connection
        .query_row(
            "SELECT client_to_server_ack_sequence, server_to_client_ack_sequence
             FROM client_exchange_cursors WHERE client_node_id = ?1",
            [client_node_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(|(client_to_server, server_to_client)| {
            Ok(ClientExchangeCursors {
                client_to_server_ack_sequence: from_sql_integer(
                    client_to_server,
                    "client-to-server ack sequence",
                )?,
                server_to_client_ack_sequence: from_sql_integer(
                    server_to_client,
                    "server-to-client ack sequence",
                )?,
            })
        })
        .transpose()
}

fn validate_schema(connection: &rusqlite::Connection) -> Result<(), ClientRegistryError> {
    validate_columns(
        connection,
        "client_nodes",
        &[
            "client_node_id",
            "public_client_id",
            "display_name",
            "platform",
            "architecture",
            "client_version",
            "device_credential_digest",
            "current_instance_id",
            "presence_state",
            "accepting_connections",
            "lock_state",
            "max_concurrent_worker_sessions",
            "reported_running_worker_sessions",
            "last_heartbeat_at",
            "created_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "client_exchange_cursors",
        &[
            "client_node_id",
            "client_to_server_ack_sequence",
            "server_to_client_ack_sequence",
        ],
    )
}

fn validate_columns(
    connection: &rusqlite::Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), ClientRegistryError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            ClientRegistryErrorKind::CorruptState,
            "client registry schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_client_node_id(value: &str) -> Result<(), ClientRegistryError> {
    validate_crockford_id(value, "cnd_", "client node id")
}

fn validate_client_instance_id(value: &str) -> Result<(), ClientRegistryError> {
    validate_crockford_id(value, "cix_", "client instance id")
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), ClientRegistryError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(error(
            ClientRegistryErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(error(
            ClientRegistryErrorKind::InvalidInput,
            format!("{label} is not canonical"),
        ));
    }
    Ok(())
}

fn validate_public_client_id(value: &str) -> Result<(), ClientRegistryError> {
    if value.len() < 9 || value.len() > 12 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(error(
            ClientRegistryErrorKind::InvalidInput,
            "public client id must contain 9 to 12 digits",
        ));
    }
    Ok(())
}

fn validate_client_version(value: &str) -> Result<(), ClientRegistryError> {
    let invalid = || {
        error(
            ClientRegistryErrorKind::InvalidInput,
            "client version is not a canonical semantic version",
        )
    };
    if value.is_empty() || value.len() > MAX_CLIENT_VERSION_BYTES {
        return Err(invalid());
    }
    let split_at = value.find(['-', '+']).unwrap_or(value.len());
    let (core, suffix) = (&value[..split_at], value.get(split_at + 1..));
    let mut components = 0;
    for component in core.split('.') {
        components += 1;
        if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
    }
    if components != 3 {
        return Err(invalid());
    }
    match suffix {
        Some("") => Err(invalid()),
        Some(suffix) => {
            if suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
            {
                Ok(())
            } else {
                Err(invalid())
            }
        }
        None => Ok(()),
    }
}

fn validate_sha256_digest(value: &str) -> Result<(), ClientRegistryError> {
    let canonical = value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    });
    if canonical {
        Ok(())
    } else {
        Err(error(
            ClientRegistryErrorKind::InvalidInput,
            "device credential digest is not canonical SHA-256",
        ))
    }
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
fn validate_instant(value: &Instant, label: &str) -> Result<(), ClientRegistryError> {
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
            ClientRegistryErrorKind::InvalidInput,
            format!("{label} instant is not canonical"),
        ))
    }
}

fn parse_stored_instant(value: &str, label: &str) -> Result<Instant, ClientRegistryError> {
    let instant = Instant(value.to_owned());
    validate_instant(&instant, label).map(|()| instant)
}

fn validate_sequence(value: u64, label: &str) -> Result<(), ClientRegistryError> {
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ClientRegistryErrorKind::InvalidInput,
            format!("{label} sequence exceeds the safe integer range"),
        ));
    }
    Ok(())
}

fn validate_revision(value: u64) -> Result<(), ClientRegistryError> {
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ClientRegistryErrorKind::InvalidInput,
            "expected revision exceeds the safe integer range",
        ));
    }
    Ok(())
}

fn ensure_revision(
    record: &ClientNodeRecord,
    expected_revision: u64,
) -> Result<(), ClientRegistryError> {
    if record.revision != expected_revision {
        return Err(error(
            ClientRegistryErrorKind::RevisionConflict,
            "client node revision does not match expectedRevision",
        ));
    }
    Ok(())
}

fn unknown_client_node() -> ClientRegistryError {
    error(
        ClientRegistryErrorKind::UnknownClientNode,
        "client node does not exist",
    )
}

fn sql_integer(value: u64) -> Result<i64, ClientRegistryError> {
    i64::try_from(value).map_err(|_| {
        error(
            ClientRegistryErrorKind::InvalidInput,
            "numeric value exceeds the SQLite integer range",
        )
    })
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, ClientRegistryError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            ClientRegistryErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            ClientRegistryErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn storage_error(storage: &StorageError) -> ClientRegistryError {
    error(
        ClientRegistryErrorKind::Storage,
        format!("client registry storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> ClientRegistryError {
    error(
        ClientRegistryErrorKind::Storage,
        "client registry storage operation failed",
    )
}

fn error(kind: ClientRegistryErrorKind, message: impl Into<String>) -> ClientRegistryError {
    ClientRegistryError {
        kind,
        message: message.into(),
    }
}
