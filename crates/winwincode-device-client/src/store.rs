// SPDX-License-Identifier: Apache-2.0

//! Local device-client `SQLite` store holding the eleven local tables from
//! plan section 8 plus the two CLIENT-200.2 tables (`connect_code_state`,
//! `client_connection_policy`) for the dynamic connect code and the local
//! lock/new-connection policy.
//!
//! The open sequence, migration style, transaction discipline, and the
//! static-SQL-only rule deliberately mirror `crates/winwincode-storage`, the
//! authoritative local-`SQLite` storage adapter in this repository.
//!
//! Local-data boundary: rows in `repository_path_mapping` (and every other
//! absolute local path stored in worker/candidate tables) never leave the
//! device. Only the tables consumed by the `ClientControlPort` exchange —
//! `client_outbox` and the `client_inbox_cursor` — feed server traffic.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError, Weak};
use std::thread;
use std::time::{Duration, Instant as StdInstant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use winwincode_client_port::domain::{ClientLockState, ConnectCodeState};
use winwincode_client_port::exchange::{
    CompactingOutbox, FrameCodec, FrameOutbox, OutboxSnapshot, StoredFrame,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientToServerEnvelope, ClientToServerMessage,
};

/// Current schema version of the local device-client database. Version 2
/// added the server-issued `client_node_id` column to `device_identity`
/// (plan 17.1 enrollment adoption). Version 3 added the two CLIENT-200.2
/// tables (`connect_code_state`, `client_connection_policy`) holding the
/// durable state of the dynamic connect code and the local lock/new-connection
/// policy. No version-1 or version-2 database ever shipped, so older
/// databases fail closed as unsupported.
pub const CLIENT_STORE_SCHEMA_VERSION: i64 = 3;

const DATABASE_FILE_NAME: &str = "device-client.sqlite3";
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(1);
/// Largest integer exactly representable where it matters for wire cursors;
/// matches the durable-cursor bound used by `winwincode-storage`.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 200;
const MAX_URL_BYTES: usize = 2048;
const MAX_PATH_BYTES: usize = 4096;

static SQLITE_OPEN_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
// SQLite's default busy timeout accumulates requested sleeps and is not a
// wall-clock deadline. The open-only handler reads this thread-local
// monotonic deadline on every busy callback.
thread_local! {
    static SQLITE_OPEN_BUSY_DEADLINE: Cell<Option<StdInstant>> = const { Cell::new(None) };
}

/// Stable error categories exposed by the device-client store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceStoreErrorKind {
    InvalidInput,
    Conflict,
    NotFound,
    Closed,
    Adapter,
}

/// Device-client store failure with an adapter-neutral category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceStoreError {
    kind: DeviceStoreErrorKind,
    message: String,
}

impl DeviceStoreError {
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: DeviceStoreErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: DeviceStoreErrorKind::Conflict,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            kind: DeviceStoreErrorKind::NotFound,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn adapter(message: impl Into<String>) -> Self {
        Self {
            kind: DeviceStoreErrorKind::Adapter,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn closed() -> Self {
        Self {
            kind: DeviceStoreErrorKind::Closed,
            message: "device client store is closed".to_owned(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> DeviceStoreErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DeviceStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for DeviceStoreError {}

pub(crate) fn sql_error(error: rusqlite::Error) -> DeviceStoreError {
    let message = format!("device client SQLite failure: {error}");
    drop(error);
    DeviceStoreError::adapter(message)
}

fn require_non_empty(value: &str, label: &str, max_bytes: usize) -> Result<(), DeviceStoreError> {
    if value.is_empty() {
        return Err(DeviceStoreError::invalid(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(DeviceStoreError::invalid(format!(
            "{label} must contain at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

/// One local path-mapping row (plan section 8.1).
///
/// LOCAL ONLY: `canonical_path`, `git_common_directory`, and every other
/// absolute path in this record are never uploaded to any server. The stable
/// `repository_binding_id` is the only server-visible identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathMappingRecord {
    pub repository_binding_id: String,
    pub canonical_path: String,
    pub git_common_directory: Option<String>,
    /// RFC 3339 timestamp of the last successful canonicalization, or `None`
    /// before the first successful scan.
    pub last_canonicalized_at: Option<String>,
    /// Local lifecycle vocabulary owned by later device-client lanes.
    pub local_state: String,
}

/// One durable pending (or published) client-to-server envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientOutboxEntry {
    pub outbox_sequence: u64,
    pub message_id: String,
    pub client_node_id: String,
    pub client_instance_id: String,
    pub envelope_sequence: u64,
    /// Wire `kind` string from the `ClientControlPort` contract.
    pub kind: String,
    /// Canonical `serde_json` encoding of the full `ClientToServerEnvelope`.
    pub payload: Vec<u8>,
    pub occurred_at: String,
}

/// One strictly forward-only server-to-client replay cursor update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientInboxCursorUpdate {
    pub server_profile_id: String,
    pub last_sequence: u64,
    pub last_message_id: Option<String>,
    /// RFC 3339 timestamp supplied by the caller's clock.
    pub updated_at: String,
}

/// Durable state of the currently published dynamic connect code
/// (CLIENT-200.2, plan 11.3).
///
/// LOCAL ONLY: the plaintext code never persists — this record carries the
/// `sha256:` digest plus the metadata needed to answer
/// `client.access.challenge` with the code-generation verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectCodeStateRecord {
    pub connect_code_id: String,
    /// `sha256:` digest of the 8-digit plaintext code.
    pub code_digest: String,
    /// Monotonic publication generation; a refresh replaces the row with
    /// `generation + 1`, invalidating every older generation.
    pub generation: u64,
    /// `clientInstanceId` of the process that published the code.
    pub issued_by_instance_id: String,
    /// RFC 3339 expiry of the code (default 120 seconds after issuance).
    pub expires_at: String,
    /// Lifecycle state (`active`, or `revoked` after a local disable).
    pub state: ConnectCodeState,
    pub created_at: String,
    pub updated_at: String,
}

/// Durable local connection policy (CLIENT-200.2, plan 11.1/12.1).
///
/// Mirrored into every `client.hello` / `client.heartbeat` report and
/// enforced against `client.access.challenge`: while the node is locked or
/// new connections are disabled, every challenge is rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionPolicyRecord {
    pub accepting_connections: bool,
    pub lock_state: ClientLockState,
    /// RFC 3339 timestamp of the last policy change.
    pub updated_at: String,
}

/// One durable server-to-client replay cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientInboxCursor {
    pub server_profile_id: String,
    pub last_sequence: u64,
    pub last_message_id: Option<String>,
    pub updated_at: String,
}

/// The exchange sender stream the durable outbox is currently bound to.
///
/// The `ClientControlPort` contract keys the client-to-server sequence
/// stream by `clientNodeId` (`sequence 按 clientNodeId 分流`), so outbox
/// trait operations scope their SQL by node; the bound `clientInstanceId`
/// stamps newly appended rows for the current process launch.
#[derive(Clone, Debug, Eq, PartialEq)]
struct OutboxStream {
    client_node_id: String,
    client_instance_id: String,
}

/// Local `SQLite` implementation of the device-client store.
pub struct DeviceStore {
    connection: Option<Connection>,
    read_connection: Mutex<Connection>,
    database_path: PathBuf,
    outbox_stream: Option<OutboxStream>,
}

impl DeviceStore {
    /// Opens the local database and applies all schema migrations before return.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the directory, connection,
    /// durability settings, or schema migration cannot be prepared.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, DeviceStoreError> {
        let open_deadline = StdInstant::now() + SQLITE_OPEN_TIMEOUT;
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|error| {
            DeviceStoreError::adapter(format!("failed to create the data directory: {error}"))
        })?;
        let canonical_data_directory = fs::canonicalize(data_directory).map_err(|error| {
            DeviceStoreError::adapter(format!("failed to resolve the data directory: {error}"))
        })?;
        let database_path = canonical_data_directory.join(DATABASE_FILE_NAME);
        // SQLite schema and journal-mode setup acquire database-wide locks.
        // Serialize only callers opening the same canonical database;
        // unrelated stores must not consume one another's bounded
        // initialization window.
        let open_lock = sqlite_open_lock(&database_path, open_deadline)?;
        let _open_guard = acquire_mutex_before_open_deadline(&open_lock, open_deadline)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
        let mut connection =
            Connection::open_with_flags(&database_path, flags).map_err(sql_error)?;
        set_open_busy_deadline(&connection, open_deadline)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        set_open_busy_deadline(&connection, open_deadline)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error)?;
        set_open_busy_deadline(&connection, open_deadline)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        set_open_busy_deadline(&connection, open_deadline)?;
        apply_migrations(&mut connection)?;

        let read_connection =
            Connection::open_with_flags(&database_path, flags).map_err(sql_error)?;
        set_open_busy_deadline(&read_connection, open_deadline)?;
        read_connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        set_open_busy_deadline(&read_connection, open_deadline)?;
        read_connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        connection
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(sql_error)?;
        read_connection
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(sql_error)?;

        Ok(Self {
            connection: Some(connection),
            read_connection: Mutex::new(read_connection),
            database_path,
            outbox_stream: None,
        })
    }

    /// Deterministically closes the store and checkpoints its write-ahead log.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when checkpointing or close fails.
    pub fn close(self) -> Result<(), DeviceStoreError> {
        let DeviceStore {
            mut connection,
            read_connection,
            database_path: _,
            outbox_stream: _,
        } = self;
        let read_connection = read_connection.into_inner().map_err(|_| {
            DeviceStoreError::adapter("SQLite device client read connection lock is poisoned")
        })?;
        read_connection
            .close()
            .map_err(|(_, error)| sql_error(error))?;
        let connection = connection.take().ok_or_else(DeviceStoreError::closed)?;
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(sql_error)?;
        connection.close().map_err(|(_, error)| sql_error(error))?;
        Ok(())
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub(crate) fn connection(&self) -> Result<&Connection, DeviceStoreError> {
        self.connection
            .as_ref()
            .ok_or_else(DeviceStoreError::closed)
    }

    pub(crate) fn connection_mut(&mut self) -> Result<&mut Connection, DeviceStoreError> {
        self.connection
            .as_mut()
            .ok_or_else(DeviceStoreError::closed)
    }

    /// Inserts or replaces one local path-mapping row.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::InvalidInput`] for an empty or
    /// overlong identity, path, or local state value, and an adapter-neutral
    /// error when the write fails or the store is closed.
    pub fn put_path_mapping(&mut self, record: &PathMappingRecord) -> Result<(), DeviceStoreError> {
        require_non_empty(
            &record.repository_binding_id,
            "repository binding id",
            MAX_ID_BYTES,
        )?;
        require_non_empty(&record.canonical_path, "canonical path", MAX_PATH_BYTES)?;
        require_non_empty(&record.local_state, "local state", MAX_ID_BYTES)?;
        if let Some(git_common_directory) = &record.git_common_directory {
            require_non_empty(git_common_directory, "git common directory", MAX_PATH_BYTES)?;
        }
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        transaction
            .execute(
                "INSERT INTO repository_path_mapping \
                 (repository_binding_id, canonical_path, git_common_directory, \
                  last_canonicalized_at, local_state) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (repository_binding_id) DO UPDATE SET \
                 canonical_path = excluded.canonical_path, \
                 git_common_directory = excluded.git_common_directory, \
                 last_canonicalized_at = excluded.last_canonicalized_at, \
                 local_state = excluded.local_state",
                params![
                    record.repository_binding_id,
                    record.canonical_path,
                    record.git_common_directory,
                    record.last_canonicalized_at,
                    record.local_state,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)
    }

    /// Loads one local path-mapping row by its repository binding id.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or the store is
    /// closed.
    pub fn path_mapping(
        &self,
        repository_binding_id: &str,
    ) -> Result<Option<PathMappingRecord>, DeviceStoreError> {
        self.connection()?
            .query_row(
                "SELECT repository_binding_id, canonical_path, git_common_directory, \
                 last_canonicalized_at, local_state \
                 FROM repository_path_mapping WHERE repository_binding_id = ?1",
                params![repository_binding_id],
                row_to_path_mapping,
            )
            .optional()
            .map_err(sql_error)
    }

    /// Loads every local path-mapping row in binding-id order.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or the store is
    /// closed.
    pub fn path_mappings(&self) -> Result<Vec<PathMappingRecord>, DeviceStoreError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT repository_binding_id, canonical_path, git_common_directory, \
                 last_canonicalized_at, local_state \
                 FROM repository_path_mapping ORDER BY repository_binding_id",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], row_to_path_mapping)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    /// Deletes one local path-mapping row.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the delete fails or the store is
    /// closed.
    pub fn delete_path_mapping(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<bool, DeviceStoreError> {
        let changed = self
            .connection_mut()?
            .execute(
                "DELETE FROM repository_path_mapping WHERE repository_binding_id = ?1",
                params![repository_binding_id],
            )
            .map_err(sql_error)?;
        Ok(changed > 0)
    }

    /// Advances the durable server-to-client replay cursor for one server.
    ///
    /// The cursor is strictly forward-only: an update whose
    /// [`ClientInboxCursorUpdate::last_sequence`] is below the stored position
    /// is rejected, and an update at the stored position is accepted only
    /// when it names the same message id (idempotent replay).
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::Conflict`] for a backwards or diverging
    /// update and an adapter-neutral error when the write fails or the store
    /// is closed.
    pub fn advance_inbox_cursor(
        &mut self,
        update: &ClientInboxCursorUpdate,
    ) -> Result<(), DeviceStoreError> {
        require_non_empty(&update.server_profile_id, "server profile id", MAX_ID_BYTES)?;
        require_non_empty(&update.updated_at, "updated at", MAX_ID_BYTES)?;
        if update.last_sequence > MAX_SAFE_INTEGER {
            return Err(DeviceStoreError::invalid(
                "inbox cursor last_sequence exceeds the SQLite integer range",
            ));
        }
        if update.last_sequence == 0 && update.last_message_id.is_some() {
            return Err(DeviceStoreError::invalid(
                "inbox cursor position 0 must not name a message id",
            ));
        }
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let current: Option<(i64, Option<String>)> = transaction
            .query_row(
                "SELECT last_sequence, last_message_id FROM client_inbox_cursor \
                 WHERE server_profile_id = ?1",
                params![update.server_profile_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        if let Some((current_sequence, current_message_id)) = current {
            let current_sequence = u64::try_from(current_sequence)
                .map_err(|_| DeviceStoreError::adapter("stored inbox cursor is negative"))?;
            if update.last_sequence < current_sequence
                || (update.last_sequence == current_sequence
                    && update.last_message_id != current_message_id)
            {
                return Err(DeviceStoreError::conflict(
                    "inbox cursor update must not move behind the durable position",
                ));
            }
            if update.last_sequence == current_sequence {
                return Ok(());
            }
        }
        transaction
            .execute(
                "INSERT INTO client_inbox_cursor \
                 (server_profile_id, last_sequence, last_message_id, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (server_profile_id) DO UPDATE SET \
                 last_sequence = excluded.last_sequence, \
                 last_message_id = excluded.last_message_id, \
                 updated_at = excluded.updated_at",
                params![
                    update.server_profile_id,
                    i64::try_from(update.last_sequence).map_err(|_| DeviceStoreError::adapter(
                        "inbox cursor sequence is outside the SQLite integer range"
                    ))?,
                    update.last_message_id,
                    update.updated_at,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)
    }

    /// Loads one durable server-to-client replay cursor.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or the store is
    /// closed.
    pub fn inbox_cursor(
        &self,
        server_profile_id: &str,
    ) -> Result<Option<ClientInboxCursor>, DeviceStoreError> {
        self.connection()?
            .query_row(
                "SELECT server_profile_id, last_sequence, last_message_id, updated_at \
                 FROM client_inbox_cursor WHERE server_profile_id = ?1",
                params![server_profile_id],
                |row| {
                    let last_sequence: i64 = row.get(1)?;
                    let last_sequence = u64::try_from(last_sequence).map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Integer,
                            "stored inbox cursor is negative".into(),
                        )
                    })?;
                    Ok(ClientInboxCursor {
                        server_profile_id: row.get(0)?,
                        last_sequence,
                        last_message_id: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(sql_error)
    }

    /// Appends one pending client-to-server envelope.
    ///
    /// The envelope is stored as its canonical `serde_json` encoding together
    /// with the queried envelope frame fields. The per-sender
    /// `sequence` must be strictly greater than every stored sequence for the
    /// same `clientNodeId`/`clientInstanceId` pair, mirroring the wire
    /// contract's replay protection.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::InvalidInput`] for a foreign schema
    /// version or an empty identity, [`DeviceStoreErrorKind::Conflict`] for a
    /// duplicate `messageId` or a non-advancing per-sender sequence, and an
    /// adapter-neutral error when the write fails or the store is closed.
    pub fn append_outbox_envelope(
        &mut self,
        envelope: &ClientToServerEnvelope,
        kind: &str,
    ) -> Result<u64, DeviceStoreError> {
        if envelope.schema_version != CLIENT_CONTROL_PORT_SCHEMA_VERSION {
            return Err(DeviceStoreError::invalid(
                "outbox envelope schema version is not the current ClientControlPort contract",
            ));
        }
        require_non_empty(&envelope.client_node_id, "client node id", MAX_ID_BYTES)?;
        require_non_empty(
            &envelope.client_instance_id,
            "client instance id",
            MAX_ID_BYTES,
        )?;
        require_non_empty(&envelope.message_id, "message id", MAX_ID_BYTES)?;
        require_non_empty(&envelope.occurred_at, "occurred at", MAX_ID_BYTES)?;
        require_non_empty(kind, "outbox kind", MAX_ID_BYTES)?;
        if envelope.sequence == 0 || envelope.sequence > MAX_SAFE_INTEGER {
            return Err(DeviceStoreError::invalid(
                "outbox envelope sequence must be a positive SQLite-range integer",
            ));
        }
        let payload = serde_json::to_vec(envelope).map_err(|error| {
            DeviceStoreError::invalid(format!("envelope is not encodable: {error}"))
        })?;

        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let highest: Option<i64> = transaction
            .query_row(
                "SELECT MAX(envelope_sequence) FROM client_outbox \
                 WHERE client_node_id = ?1 AND client_instance_id = ?2",
                params![envelope.client_node_id, envelope.client_instance_id],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if highest.is_some_and(|highest| {
            u64::try_from(highest).is_ok_and(|highest| highest >= envelope.sequence)
        }) {
            return Err(DeviceStoreError::conflict(
                "outbox envelope sequence must strictly advance per sender",
            ));
        }
        insert_outbox_row(&transaction, envelope, kind, &payload)?;
        let outbox_sequence = transaction.last_insert_rowid();
        transaction.commit().map_err(sql_error)?;
        u64::try_from(outbox_sequence)
            .map_err(|_| DeviceStoreError::adapter("outbox sequence is negative"))
    }

    /// Loads unpublished client-to-server envelopes in durable order.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or the store is
    /// closed.
    pub fn pending_outbox_envelopes(&self) -> Result<Vec<ClientOutboxEntry>, DeviceStoreError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT outbox_sequence, message_id, client_node_id, client_instance_id, \
                 envelope_sequence, kind, payload, occurred_at \
                 FROM client_outbox WHERE published = 0 ORDER BY outbox_sequence",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], row_to_outbox_entry)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    /// Marks one envelope as published by its message id.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::NotFound`] when the message id is
    /// unknown, and an adapter-neutral error when the write fails or the
    /// store is closed.
    pub fn mark_outbox_published(&mut self, message_id: &str) -> Result<(), DeviceStoreError> {
        require_non_empty(message_id, "message id", MAX_ID_BYTES)?;
        let changed = self
            .connection_mut()?
            .execute(
                "UPDATE client_outbox SET published = 1 WHERE message_id = ?1",
                params![message_id],
            )
            .map_err(sql_error)?;
        if changed == 0 {
            return Err(DeviceStoreError::not_found(format!(
                "outbox message id {message_id} does not exist"
            )));
        }
        Ok(())
    }

    /// Loads the durable state of the currently published connect code.
    ///
    /// The plaintext code is never stored; only this digest-bearing record
    /// survives a restart so challenges stay answerable across process
    /// launches.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails, the store is
    /// closed, or the stored row disagrees with its schema vocabulary.
    pub fn connect_code_state(&self) -> Result<Option<ConnectCodeStateRecord>, DeviceStoreError> {
        self.connection()?
            .query_row(
                "SELECT connect_code_id, code_digest, generation, issued_by_instance_id, \
                 expires_at, state, created_at, updated_at \
                 FROM connect_code_state WHERE singleton = 1",
                [],
                row_to_connect_code_state,
            )
            .optional()
            .map_err(sql_error)
    }

    /// Replaces the durable connect-code state with the next publication.
    ///
    /// The replacement is generation-monotonic: a record whose
    /// [`ConnectCodeStateRecord::generation`] does not strictly advance past
    /// the stored generation is rejected, so two racing publishers can never
    /// end up with colliding generations.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::InvalidInput`] for an empty or
    /// overlong field, [`DeviceStoreErrorKind::Conflict`] for a
    /// non-advancing generation, and an adapter-neutral error when the write
    /// fails or the store is closed.
    pub fn replace_connect_code_state(
        &mut self,
        record: &ConnectCodeStateRecord,
    ) -> Result<(), DeviceStoreError> {
        validate_connect_code_record(record)?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let stored: Option<i64> = transaction
            .query_row(
                "SELECT generation FROM connect_code_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        if stored.is_some_and(|stored| {
            u64::try_from(stored).is_ok_and(|stored| stored >= record.generation)
        }) {
            return Err(DeviceStoreError::conflict(format!(
                "connect code generation {} does not advance past the stored generation",
                record.generation
            )));
        }
        transaction
            .execute(
                "INSERT INTO connect_code_state \
                 (singleton, connect_code_id, code_digest, generation, \
                  issued_by_instance_id, expires_at, state, created_at, updated_at) \
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (singleton) DO UPDATE SET \
                 connect_code_id = excluded.connect_code_id, \
                 code_digest = excluded.code_digest, \
                 generation = excluded.generation, \
                 issued_by_instance_id = excluded.issued_by_instance_id, \
                 expires_at = excluded.expires_at, \
                 state = excluded.state, \
                 created_at = excluded.created_at, \
                 updated_at = excluded.updated_at",
                params![
                    record.connect_code_id,
                    record.code_digest,
                    i64::try_from(record.generation).map_err(|_| {
                        DeviceStoreError::invalid("connect code generation is out of range")
                    })?,
                    record.issued_by_instance_id,
                    record.expires_at,
                    connect_code_state_to_str(record.state),
                    record.created_at,
                    record.updated_at,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)
    }

    /// Marks the stored connect code `revoked` (the local disable). A no-op
    /// returning `false` when no active code exists.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the write fails or the store is
    /// closed.
    pub fn revoke_connect_code_state(
        &mut self,
        revoked_at: &str,
    ) -> Result<bool, DeviceStoreError> {
        require_non_empty(revoked_at, "revoked at", MAX_ID_BYTES)?;
        let changed = self
            .connection_mut()?
            .execute(
                "UPDATE connect_code_state SET state = 'revoked', updated_at = ?1 \
                 WHERE singleton = 1 AND state = 'active'",
                params![revoked_at],
            )
            .map_err(sql_error)?;
        Ok(changed > 0)
    }

    /// Loads the durable local connection policy.
    ///
    /// `None` before the first policy write; callers default to accepting
    /// connections with an unlocked node.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or the store is
    /// closed.
    pub fn connection_policy(&self) -> Result<Option<ConnectionPolicyRecord>, DeviceStoreError> {
        self.connection()?
            .query_row(
                "SELECT accepting_connections, lock_state, updated_at \
                 FROM client_connection_policy WHERE singleton = 1",
                [],
                |row| {
                    let accepting: i64 = row.get(0)?;
                    let lock_state: String = row.get(1)?;
                    Ok(ConnectionPolicyRecord {
                        accepting_connections: accepting != 0,
                        lock_state: parse_lock_state(&lock_state)?,
                        updated_at: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(sql_error)
    }

    /// Persists the local connection policy (lock state plus whether new
    /// connections are accepted).
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::InvalidInput`] for an empty timestamp
    /// and an adapter-neutral error when the write fails or the store is
    /// closed.
    pub fn put_connection_policy(
        &mut self,
        record: &ConnectionPolicyRecord,
    ) -> Result<(), DeviceStoreError> {
        require_non_empty(&record.updated_at, "policy updated at", MAX_ID_BYTES)?;
        let connection = self.connection_mut()?;
        connection
            .execute(
                "INSERT INTO client_connection_policy \
                 (singleton, accepting_connections, lock_state, updated_at) \
                 VALUES (1, ?1, ?2, ?3) \
                 ON CONFLICT (singleton) DO UPDATE SET \
                 accepting_connections = excluded.accepting_connections, \
                 lock_state = excluded.lock_state, \
                 updated_at = excluded.updated_at",
                params![
                    i64::from(record.accepting_connections),
                    lock_state_to_str(record.lock_state),
                    record.updated_at,
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }
}

impl DeviceStore {
    /// Binds the durable outbox to one sender stream: `clientNodeId` scopes
    /// every [`FrameOutbox`] operation (the contract keys the sequence stream
    /// by node), and `clientInstanceId` stamps newly appended rows for this
    /// process launch.
    ///
    /// Must be called once per store before the daemon uses the outbox trait
    /// operations; unbound calls fail with
    /// [`DeviceStoreErrorKind::InvalidInput`].
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::InvalidInput`] for an empty node or
    /// instance identity and [`DeviceStoreErrorKind::Closed`] after the store
    /// closed.
    pub fn bind_outbox_stream(
        &mut self,
        client_node_id: &str,
        client_instance_id: &str,
    ) -> Result<(), DeviceStoreError> {
        require_non_empty(client_node_id, "client node id", MAX_ID_BYTES)?;
        require_non_empty(client_instance_id, "client instance id", MAX_ID_BYTES)?;
        self.connection()?;
        self.outbox_stream = Some(OutboxStream {
            client_node_id: client_node_id.to_owned(),
            client_instance_id: client_instance_id.to_owned(),
        });
        Ok(())
    }

    /// Re-keys the enrolled sender stream after the enrollment exchange:
    /// every acknowledged (published) row of the placeholder stream is copied
    /// under the server-assigned node id at the same stream sequence, the
    /// copies are marked published, and the outbox stream rebinds to the
    /// enrolled node.
    ///
    /// The server credits the settled enroll frames to the assigned `cnd_`
    /// stream, so the copies keep the node sequence continuous: the next
    /// appended frame continues at the sequence the server expects. Pending
    /// (unacknowledged) placeholder rows stay untouched on the placeholder
    /// stream; the daemon enqueues nothing but the enroll before the
    /// adoption, so none exist in the exchange flow.
    ///
    /// Refused when the enrolled node already has rows, so a replayed
    /// adoption can never duplicate the stream.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::InvalidInput`] for an empty node
    /// identity and an adapter-neutral error when the store is closed, a
    /// stored row disagrees with its encoded envelope, the enrolled stream
    /// already exists, or the write fails.
    pub fn adopt_enrolled_stream(
        &mut self,
        placeholder_node_id: &str,
        enrolled_node_id: &str,
    ) -> Result<(), DeviceStoreError> {
        require_non_empty(placeholder_node_id, "placeholder node id", MAX_ID_BYTES)?;
        require_non_empty(enrolled_node_id, "enrolled node id", MAX_ID_BYTES)?;
        let instance = self.outbox_stream()?.client_instance_id.clone();
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let existing: Option<i64> = transaction
            .query_row(
                "SELECT MAX(envelope_sequence) FROM client_outbox WHERE client_node_id = ?1",
                [enrolled_node_id],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if existing.is_some() {
            return Err(DeviceStoreError::conflict(
                "the enrolled node stream already has rows",
            ));
        }
        let published: Vec<(String, i64, String, Vec<u8>, String)> = transaction
            .prepare(
                "SELECT message_id, envelope_sequence, kind, payload, occurred_at \
                 FROM client_outbox \
                 WHERE client_node_id = ?1 AND published = 1 ORDER BY envelope_sequence",
            )
            .map_err(sql_error)?
            .query_map([placeholder_node_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)?;
        for (message_id, envelope_sequence, kind, payload, occurred_at) in published {
            let envelope: ClientToServerEnvelope =
                serde_json::from_slice(&payload).map_err(|error| {
                    DeviceStoreError::adapter(format!(
                        "published outbox row {message_id} is corrupt: {error}"
                    ))
                })?;
            if envelope.client_node_id != placeholder_node_id
                || envelope.message_id != message_id
                || envelope_kind(&envelope.message)? != kind
            {
                return Err(DeviceStoreError::adapter(format!(
                    "published outbox row {message_id} disagrees with its encoded envelope"
                )));
            }
            let sequence = u64::try_from(envelope_sequence)
                .map_err(|_| DeviceStoreError::adapter("stored outbox sequence is negative"))?;
            if envelope.sequence != sequence {
                return Err(DeviceStoreError::adapter(format!(
                    "published outbox row {message_id} disagrees with its encoded sequence"
                )));
            }
            let mut adopted = envelope;
            enrolled_node_id.clone_into(&mut adopted.client_node_id);
            let payload = serde_json::to_vec(&adopted).map_err(|error| {
                DeviceStoreError::adapter(format!(
                    "adopted outbox row for {enrolled_node_id} is not encodable: {error}"
                ))
            })?;
            transaction
                .execute(
                    "INSERT INTO client_outbox \
                     (message_id, client_node_id, client_instance_id, envelope_sequence, \
                      kind, payload, occurred_at, published) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)",
                    params![
                        format!("{enrolled_node_id}-{kind}-{sequence}"),
                        enrolled_node_id,
                        adopted.client_instance_id,
                        envelope_sequence,
                        kind,
                        payload,
                        occurred_at,
                    ],
                )
                .map_err(|error| match error {
                    rusqlite::Error::SqliteFailure(failure, _)
                        if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        DeviceStoreError::conflict("the adopted outbox message id already exists")
                    }
                    other => sql_error(other),
                })?;
        }
        transaction.commit().map_err(sql_error)?;
        self.outbox_stream = Some(OutboxStream {
            client_node_id: enrolled_node_id.to_owned(),
            client_instance_id: instance,
        });
        Ok(())
    }

    /// Inserts or replaces one server profile row.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceStoreErrorKind::InvalidInput`] for an empty or
    /// overlong identifier, URL, or display name, and an adapter-neutral
    /// error when the write fails or the store is closed.
    pub fn upsert_server_profile(
        &mut self,
        record: &ServerProfileRecord,
    ) -> Result<(), DeviceStoreError> {
        require_non_empty(&record.server_profile_id, "server profile id", MAX_ID_BYTES)?;
        require_non_empty(&record.base_url, "server base url", MAX_URL_BYTES)?;
        require_non_empty(&record.display_name, "server display name", MAX_ID_BYTES)?;
        require_non_empty(
            &record.created_at,
            "server profile created at",
            MAX_ID_BYTES,
        )?;
        if let Some(last_connected_at) = &record.last_connected_at {
            require_non_empty(last_connected_at, "server last connected at", MAX_ID_BYTES)?;
        }
        let connection = self.connection_mut()?;
        connection
            .execute(
                "INSERT INTO server_profile \
                 (server_profile_id, base_url, display_name, created_at, last_connected_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (server_profile_id) DO UPDATE SET \
                 base_url = excluded.base_url, \
                 display_name = excluded.display_name, \
                 last_connected_at = excluded.last_connected_at",
                params![
                    record.server_profile_id,
                    record.base_url,
                    record.display_name,
                    record.created_at,
                    record.last_connected_at,
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    /// Loads one server profile row by its identifier.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or the store is
    /// closed.
    pub fn server_profile(
        &self,
        server_profile_id: &str,
    ) -> Result<Option<ServerProfileRecord>, DeviceStoreError> {
        self.connection()?
            .query_row(
                "SELECT server_profile_id, base_url, display_name, created_at, \
                 last_connected_at \
                 FROM server_profile WHERE server_profile_id = ?1",
                params![server_profile_id],
                |row| {
                    Ok(ServerProfileRecord {
                        server_profile_id: row.get(0)?,
                        base_url: row.get(1)?,
                        display_name: row.get(2)?,
                        created_at: row.get(3)?,
                        last_connected_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(sql_error)
    }

    fn outbox_stream(&self) -> Result<&OutboxStream, DeviceStoreError> {
        self.outbox_stream.as_ref().ok_or_else(|| {
            DeviceStoreError::invalid(
                "the durable outbox stream is not bound; call bind_outbox_stream first",
            )
        })
    }
}

/// One durable server profile row (`server_profile` table).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerProfileRecord {
    pub server_profile_id: String,
    pub base_url: String,
    pub display_name: String,
    pub created_at: String,
    pub last_connected_at: Option<String>,
}

/// The durable sender-outbox adapter over the `client_outbox` table.
///
/// Mapping decisions (the eleven-table schema is frozen, so the adapter
/// derives everything the [`FrameOutbox`] contract needs from existing
/// columns):
///
/// - The stream scope is `clientNodeId` (`sequence 按 clientNodeId 分流`):
///   snapshot cursors and retained frames aggregate every row of the bound
///   node, whatever `clientInstanceId` produced it, and new rows carry the
///   bound launch instance.
/// - The `published` column doubles as the compaction marker: the peer
///   acknowledgement watermark is the highest `published` envelope sequence,
///   and [`CompactingOutbox::compact_through`] moves rows out of the delivery
///   set by marking them published. Rows are retained after compaction so
///   both durable cursors (acknowledgement and high-water mark) survive even
///   a fully compacted stream; retention pruning is owned by a later lane.
/// - `StoredFrame` rows are reconstructed from the stored canonical envelope
///   bytes; the payload digest is re-derived from the decoded envelope so it
///   always matches the digest convention both exchange peers compare.
impl FrameOutbox for DeviceStore {
    type Error = DeviceStoreError;

    fn load(&mut self) -> Result<Option<OutboxSnapshot>, Self::Error> {
        let stream = self.outbox_stream()?;
        let connection = self.connection()?;
        let (ack_sequence, highest_sequence) =
            outbox_stream_cursor(connection, &stream.client_node_id)?;
        let Some(highest_sequence) = highest_sequence else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare(
                "SELECT message_id, envelope_sequence, kind, payload, occurred_at \
                 FROM client_outbox \
                 WHERE client_node_id = ?1 AND published = 0 \
                 ORDER BY envelope_sequence",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(params![stream.client_node_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(sql_error)?;
        let mut frames = Vec::new();
        for row in rows {
            let (message_id, envelope_sequence, kind, payload, occurred_at) =
                row.map_err(sql_error)?;
            frames.push(stored_frame_from_row(
                &stream.client_node_id,
                &message_id,
                envelope_sequence,
                &kind,
                payload,
                &occurred_at,
            )?);
        }
        Ok(Some(OutboxSnapshot {
            ack_sequence: ack_sequence.unwrap_or(0),
            highest_sequence,
            frames,
        }))
    }

    fn append(
        &mut self,
        expected_highest_sequence: u64,
        frame: &StoredFrame,
    ) -> Result<(), Self::Error> {
        let stream = self.outbox_stream()?.clone();
        let envelope = decode_outbox_envelope(&frame.frame)?;
        if envelope.client_node_id != stream.client_node_id {
            return Err(DeviceStoreError::invalid(format!(
                "outbox frame names client node {} but the stream is bound to {}",
                envelope.client_node_id, stream.client_node_id
            )));
        }
        if envelope.client_instance_id != stream.client_instance_id {
            return Err(DeviceStoreError::invalid(format!(
                "outbox frame names client instance {} but the stream is bound to {}",
                envelope.client_instance_id, stream.client_instance_id
            )));
        }
        if envelope.message_id != frame.message_id || envelope.sequence != frame.sequence {
            return Err(DeviceStoreError::invalid(
                "outbox frame identity disagrees with its encoded envelope",
            ));
        }
        let identity = FrameCodec::envelope_identity(&envelope).map_err(|error| {
            DeviceStoreError::invalid(format!("outbox frame digest: {error:?}"))
        })?;
        if identity.payload_digest != frame.payload_digest {
            return Err(DeviceStoreError::invalid(
                "outbox frame payload digest does not match its encoded envelope",
            ));
        }
        let kind = envelope_kind(&envelope.message)?;
        let payload = serde_json::to_vec(&envelope).map_err(|error| {
            DeviceStoreError::invalid(format!("outbox envelope is not encodable: {error}"))
        })?;

        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let (_, stored_highest) = outbox_stream_cursor(&transaction, &stream.client_node_id)?;
        if stored_highest.unwrap_or(0) != expected_highest_sequence {
            return Err(DeviceStoreError::conflict(format!(
                "outbox append expected the durable high-water mark \
                 {expected_highest_sequence}, found {}",
                stored_highest.unwrap_or(0)
            )));
        }
        insert_outbox_row(&transaction, &envelope, &kind, &payload)?;
        transaction.commit().map_err(sql_error)
    }

    fn record_acknowledgement(
        &mut self,
        expected_ack_sequence: u64,
        ack_sequence: u64,
    ) -> Result<(), Self::Error> {
        let stream = self.outbox_stream()?.clone();
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let (stored_ack, stored_highest) =
            outbox_stream_cursor(&transaction, &stream.client_node_id)?;
        if stored_ack.unwrap_or(0) != expected_ack_sequence {
            return Err(DeviceStoreError::conflict(format!(
                "outbox acknowledgement raced with the durable cursor: expected \
                 {expected_ack_sequence}, found {}",
                stored_ack.unwrap_or(0)
            )));
        }
        let highest = stored_highest.unwrap_or(0);
        if ack_sequence > highest {
            return Err(DeviceStoreError::conflict(format!(
                "outbox acknowledgement {ack_sequence} is beyond the retained \
                 high-water mark {highest}"
            )));
        }
        transaction
            .execute(
                "UPDATE client_outbox SET published = 1 \
                 WHERE client_node_id = ?1 AND envelope_sequence <= ?2",
                params![stream.client_node_id, i64_prefix(ack_sequence)?],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)
    }
}

impl CompactingOutbox for DeviceStore {
    fn compact_through(&mut self, ack_sequence: u64) -> Result<usize, Self::Error> {
        let stream = self.outbox_stream()?.clone();
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let (_, stored_highest) = outbox_stream_cursor(&transaction, &stream.client_node_id)?;
        let highest = stored_highest.unwrap_or(0);
        if ack_sequence > highest {
            return Err(DeviceStoreError::conflict(format!(
                "outbox compaction {ack_sequence} is beyond the retained \
                 high-water mark {highest}"
            )));
        }
        let changed = transaction
            .execute(
                "UPDATE client_outbox SET published = 1 \
                 WHERE client_node_id = ?1 AND published = 0 AND envelope_sequence <= ?2",
                params![stream.client_node_id, i64_prefix(ack_sequence)?],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(changed)
    }
}

/// Reads the durable node-stream cursors: `(acknowledgement, highest)`.
///
/// `None` means the node stream has no rows yet.
fn outbox_stream_cursor(
    connection: &Connection,
    client_node_id: &str,
) -> Result<(Option<u64>, Option<u64>), DeviceStoreError> {
    let to_cursor = |stored: Option<i64>| {
        stored
            .map(u64::try_from)
            .transpose()
            .map_err(|_| DeviceStoreError::adapter("stored outbox sequence is negative"))
    };
    let highest: Option<i64> = connection
        .query_row(
            "SELECT MAX(envelope_sequence) FROM client_outbox WHERE client_node_id = ?1",
            params![client_node_id],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let acknowledgement: Option<i64> = connection
        .query_row(
            "SELECT MAX(envelope_sequence) FROM client_outbox \
             WHERE client_node_id = ?1 AND published = 1",
            params![client_node_id],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    Ok((to_cursor(acknowledgement)?, to_cursor(highest)?))
}

/// Inserts one pending (unpublished) outbox row inside an open transaction.
fn insert_outbox_row(
    transaction: &rusqlite::Transaction<'_>,
    envelope: &ClientToServerEnvelope,
    kind: &str,
    payload: &[u8],
) -> Result<(), DeviceStoreError> {
    transaction
        .execute(
            "INSERT INTO client_outbox \
             (message_id, client_node_id, client_instance_id, envelope_sequence, \
              kind, payload, occurred_at, published) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![
                envelope.message_id,
                envelope.client_node_id,
                envelope.client_instance_id,
                i64::try_from(envelope.sequence).map_err(|_| DeviceStoreError::adapter(
                    "envelope sequence is outside the SQLite integer range"
                ))?,
                kind,
                payload,
                envelope.occurred_at,
            ],
        )
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(failure, _)
                if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                DeviceStoreError::conflict(format!(
                    "outbox message id {} already exists",
                    envelope.message_id
                ))
            }
            other => sql_error(other),
        })?;
    Ok(())
}

/// Converts a wire sequence into its SQLite integer encoding.
fn i64_prefix(sequence: u64) -> Result<i64, DeviceStoreError> {
    i64::try_from(sequence).map_err(|_| {
        DeviceStoreError::adapter("outbox sequence is outside the SQLite integer range")
    })
}

/// Decodes one stored canonical envelope, rejecting foreign schema versions.
fn decode_outbox_envelope(payload: &[u8]) -> Result<ClientToServerEnvelope, DeviceStoreError> {
    let envelope: ClientToServerEnvelope = serde_json::from_slice(payload).map_err(|error| {
        DeviceStoreError::invalid(format!("stored envelope is corrupt: {error}"))
    })?;
    if envelope.schema_version != CLIENT_CONTROL_PORT_SCHEMA_VERSION {
        return Err(DeviceStoreError::invalid(
            "stored envelope schema version is not the current ClientControlPort contract",
        ));
    }
    Ok(envelope)
}

/// Extracts the wire `kind` string of one typed message.
pub(crate) fn envelope_kind(message: &ClientToServerMessage) -> Result<String, DeviceStoreError> {
    let value = serde_json::to_value(message)
        .map_err(|error| DeviceStoreError::invalid(format!("message is not encodable: {error}")))?;
    value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| DeviceStoreError::invalid("encoded message carries no kind string"))
}

/// Rebuilds one [`StoredFrame`] from an unpublished `client_outbox` row,
/// re-deriving the payload digest from the stored canonical envelope and
/// fail-closing on any disagreement between the row columns and the decoded
/// envelope.
fn stored_frame_from_row(
    client_node_id: &str,
    message_id: &str,
    envelope_sequence: i64,
    kind: &str,
    payload: Vec<u8>,
    occurred_at: &str,
) -> Result<StoredFrame, DeviceStoreError> {
    let envelope = decode_outbox_envelope(&payload)?;
    if envelope.client_node_id != client_node_id
        || envelope.message_id != message_id
        || envelope.occurred_at != occurred_at
    {
        return Err(DeviceStoreError::adapter(
            "stored outbox row disagrees with its encoded envelope identity",
        ));
    }
    let sequence = u64::try_from(envelope_sequence)
        .map_err(|_| DeviceStoreError::adapter("stored outbox sequence is negative"))?;
    if envelope.sequence != sequence {
        return Err(DeviceStoreError::adapter(
            "stored outbox row disagrees with its encoded envelope sequence",
        ));
    }
    if envelope_kind(&envelope.message)? != kind {
        return Err(DeviceStoreError::adapter(
            "stored outbox row disagrees with its encoded envelope kind",
        ));
    }
    let identity = FrameCodec::envelope_identity(&envelope)
        .map_err(|error| DeviceStoreError::adapter(format!("stored frame digest: {error:?}")))?;
    Ok(StoredFrame {
        message_id: identity.message_id,
        sequence: identity.sequence,
        payload_digest: identity.payload_digest,
        frame: payload,
    })
}

fn row_to_path_mapping(row: &rusqlite::Row<'_>) -> rusqlite::Result<PathMappingRecord> {
    Ok(PathMappingRecord {
        repository_binding_id: row.get(0)?,
        canonical_path: row.get(1)?,
        git_common_directory: row.get(2)?,
        last_canonicalized_at: row.get(3)?,
        local_state: row.get(4)?,
    })
}

fn row_to_outbox_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClientOutboxEntry> {
    let outbox_sequence: i64 = row.get(0)?;
    let envelope_sequence: i64 = row.get(4)?;
    Ok(ClientOutboxEntry {
        outbox_sequence: u64::try_from(outbox_sequence).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                "stored outbox sequence is negative".into(),
            )
        })?,
        message_id: row.get(1)?,
        client_node_id: row.get(2)?,
        client_instance_id: row.get(3)?,
        envelope_sequence: u64::try_from(envelope_sequence).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                "stored envelope sequence is negative".into(),
            )
        })?,
        kind: row.get(5)?,
        payload: row.get(6)?,
        occurred_at: row.get(7)?,
    })
}

fn row_to_connect_code_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConnectCodeStateRecord> {
    let generation: i64 = row.get(2)?;
    Ok(ConnectCodeStateRecord {
        connect_code_id: row.get(0)?,
        code_digest: row.get(1)?,
        generation: u64::try_from(generation).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                "stored connect code generation is negative".into(),
            )
        })?,
        issued_by_instance_id: row.get(3)?,
        expires_at: row.get(4)?,
        state: parse_connect_code_state(&row.get::<_, String>(5)?)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn parse_connect_code_state(value: &str) -> rusqlite::Result<ConnectCodeState> {
    match value {
        "active" => Ok(ConnectCodeState::Active),
        "consumed" => Ok(ConnectCodeState::Consumed),
        "expired" => Ok(ConnectCodeState::Expired),
        "revoked" => Ok(ConnectCodeState::Revoked),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("stored connect code state {other} is not a lifecycle state").into(),
        )),
    }
}

const fn connect_code_state_to_str(state: ConnectCodeState) -> &'static str {
    match state {
        ConnectCodeState::Active => "active",
        ConnectCodeState::Consumed => "consumed",
        ConnectCodeState::Expired => "expired",
        ConnectCodeState::Revoked => "revoked",
    }
}

fn parse_lock_state(value: &str) -> rusqlite::Result<ClientLockState> {
    match value {
        "unlocked" => Ok(ClientLockState::Unlocked),
        "locked" => Ok(ClientLockState::Locked),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("stored lock state {other} is not a lock state").into(),
        )),
    }
}

const fn lock_state_to_str(state: ClientLockState) -> &'static str {
    match state {
        ClientLockState::Unlocked => "unlocked",
        ClientLockState::Locked => "locked",
    }
}

/// Validates the durable connect-code record fields before any write.
fn validate_connect_code_record(record: &ConnectCodeStateRecord) -> Result<(), DeviceStoreError> {
    require_non_empty(&record.connect_code_id, "connect code id", MAX_ID_BYTES)?;
    require_non_empty(&record.code_digest, "connect code digest", MAX_ID_BYTES)?;
    require_non_empty(
        &record.issued_by_instance_id,
        "issued by instance id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&record.expires_at, "connect code expires at", MAX_ID_BYTES)?;
    require_non_empty(&record.created_at, "connect code created at", MAX_ID_BYTES)?;
    require_non_empty(&record.updated_at, "connect code updated at", MAX_ID_BYTES)?;
    if record.generation == 0 {
        return Err(DeviceStoreError::invalid(
            "connect code generation must be positive",
        ));
    }
    Ok(())
}

const STORE_SCHEMA: &str = r"
CREATE TABLE device_identity (
    device_id TEXT PRIMARY KEY NOT NULL,
    client_node_id TEXT NOT NULL DEFAULT '',
    public_client_id TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    platform TEXT NOT NULL,
    architecture TEXT NOT NULL,
    client_version TEXT NOT NULL,
    current_instance_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0)
);
CREATE TABLE device_credential (
    device_id TEXT PRIMARY KEY NOT NULL REFERENCES device_identity (device_id) ON DELETE CASCADE,
    credential_secret BLOB NOT NULL CHECK (length(credential_secret) = 32),
    credential_digest TEXT NOT NULL CHECK (length(credential_digest) = 71),
    credential_generation INTEGER NOT NULL CHECK (credential_generation > 0),
    rotated_at TEXT NOT NULL
);
CREATE TABLE server_profile (
    server_profile_id TEXT PRIMARY KEY NOT NULL,
    base_url TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_connected_at TEXT
);
-- LOCAL ONLY (plan section 8.1): absolute paths in this table are never
-- uploaded to a server. The repository binding id is the only server-visible
-- identity for a locally registered repository checkout.
CREATE TABLE repository_path_mapping (
    repository_binding_id TEXT PRIMARY KEY NOT NULL,
    canonical_path TEXT NOT NULL,
    git_common_directory TEXT,
    last_canonicalized_at TEXT,
    local_state TEXT NOT NULL
);
CREATE TABLE repository_local_state (
    repository_binding_id TEXT PRIMARY KEY NOT NULL,
    dirty_state TEXT NOT NULL,
    availability TEXT NOT NULL,
    head_commit TEXT,
    last_scanned_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE TABLE occupancy_mirror (
    occupancy_lease_id TEXT PRIMARY KEY NOT NULL,
    repository_binding_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    state TEXT NOT NULL,
    fencing_token INTEGER NOT NULL CHECK (fencing_token >= 0),
    acquired_at TEXT,
    expires_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX occupancy_mirror_by_repository
    ON occupancy_mirror (repository_binding_id, updated_at);
-- Plan section 8.2: a PID alone is never sufficient because PIDs are reused;
-- every row additionally binds the process start identity (platform handle).
CREATE TABLE worker_process_registry (
    worker_session_id TEXT PRIMARY KEY NOT NULL,
    worker_id TEXT NOT NULL,
    worker_instance_id TEXT NOT NULL,
    pid INTEGER NOT NULL CHECK (pid > 0),
    process_start_identity TEXT NOT NULL,
    repository_binding_id TEXT NOT NULL,
    occupancy_lease_id TEXT NOT NULL,
    launch_grant_id TEXT NOT NULL,
    data_directory TEXT NOT NULL,
    state TEXT NOT NULL,
    last_observed_at TEXT NOT NULL
);
CREATE INDEX worker_process_registry_by_repository
    ON worker_process_registry (repository_binding_id, state);
CREATE TABLE worker_launch_receipts (
    launch_grant_id TEXT PRIMARY KEY NOT NULL,
    worker_session_id TEXT NOT NULL,
    ack_status TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    receipt_payload BLOB NOT NULL,
    received_at TEXT NOT NULL
);
CREATE INDEX worker_launch_receipts_by_session
    ON worker_launch_receipts (worker_session_id, received_at);
-- LOCAL ONLY: local candidate git refs and workspace paths never leave the
-- device; only the stable candidate id is server-visible.
CREATE TABLE candidate_local_refs (
    candidate_id TEXT PRIMARY KEY NOT NULL,
    worker_session_id TEXT NOT NULL,
    repository_binding_id TEXT NOT NULL,
    local_git_ref TEXT NOT NULL,
    local_state TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX candidate_local_refs_by_session
    ON candidate_local_refs (worker_session_id, created_at);
CREATE TABLE client_outbox (
    outbox_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT NOT NULL UNIQUE,
    client_node_id TEXT NOT NULL,
    client_instance_id TEXT NOT NULL,
    envelope_sequence INTEGER NOT NULL CHECK (envelope_sequence > 0),
    kind TEXT NOT NULL,
    payload BLOB NOT NULL,
    occurred_at TEXT NOT NULL,
    published INTEGER NOT NULL DEFAULT 0 CHECK (published IN (0, 1))
);
CREATE INDEX client_outbox_pending_sequence
    ON client_outbox (published, outbox_sequence);
CREATE TABLE client_inbox_cursor (
    server_profile_id TEXT PRIMARY KEY NOT NULL,
    last_sequence INTEGER NOT NULL CHECK (last_sequence >= 0),
    last_message_id TEXT,
    updated_at TEXT NOT NULL
);
-- CLIENT-200.2 (plan 11.3): local state of the currently published dynamic
-- connect code. The plaintext code never persists; only its sha256 digest
-- plus the metadata needed to answer client.access.challenge survive a
-- restart. A refresh replaces the single row at generation + 1.
CREATE TABLE connect_code_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    connect_code_id TEXT NOT NULL,
    code_digest TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    issued_by_instance_id TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('active', 'consumed', 'expired', 'revoked')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
-- CLIENT-200.2 (plan 11.1/12.1): durable local connection policy, mirrored
-- into client.hello / client.heartbeat and enforced against
-- client.access.challenge.
CREATE TABLE client_connection_policy (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    accepting_connections INTEGER NOT NULL CHECK (accepting_connections IN (0, 1)),
    lock_state TEXT NOT NULL CHECK (lock_state IN ('unlocked', 'locked')),
    updated_at TEXT NOT NULL
);
";

fn apply_migrations(connection: &mut Connection) -> Result<(), DeviceStoreError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    if !matches!(version, 0 | CLIENT_STORE_SCHEMA_VERSION) {
        return Err(DeviceStoreError::adapter(format!(
            "unsupported schema version {version}"
        )));
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    if version == 0 {
        transaction.execute_batch(STORE_SCHEMA).map_err(sql_error)?;
    }
    validate_store_schema(&transaction)?;
    transaction
        .pragma_update(None, "user_version", CLIENT_STORE_SCHEMA_VERSION)
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)?;

    let migrated_version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    if migrated_version != CLIENT_STORE_SCHEMA_VERSION {
        return Err(DeviceStoreError::adapter(format!(
            "unsupported schema version {migrated_version}"
        )));
    }
    Ok(())
}

/// Canonical tables, their static `PRAGMA table_info` query, and their
/// canonical column names, validated after creation and before every open
/// serves traffic. The pragma queries are static per table so the adapter
/// keeps the no-dynamic-SQL-identifier rule of `winwincode-storage`.
const STORE_SCHEMA_COLUMNS: &[(&str, &str, &[&str])] = &[
    (
        "device_identity",
        "PRAGMA table_info(device_identity)",
        &[
            "device_id",
            "client_node_id",
            "public_client_id",
            "display_name",
            "platform",
            "architecture",
            "client_version",
            "current_instance_id",
            "created_at",
            "revision",
        ],
    ),
    (
        "device_credential",
        "PRAGMA table_info(device_credential)",
        &[
            "device_id",
            "credential_secret",
            "credential_digest",
            "credential_generation",
            "rotated_at",
        ],
    ),
    (
        "server_profile",
        "PRAGMA table_info(server_profile)",
        &[
            "server_profile_id",
            "base_url",
            "display_name",
            "created_at",
            "last_connected_at",
        ],
    ),
    (
        "repository_path_mapping",
        "PRAGMA table_info(repository_path_mapping)",
        &[
            "repository_binding_id",
            "canonical_path",
            "git_common_directory",
            "last_canonicalized_at",
            "local_state",
        ],
    ),
    (
        "repository_local_state",
        "PRAGMA table_info(repository_local_state)",
        &[
            "repository_binding_id",
            "dirty_state",
            "availability",
            "head_commit",
            "last_scanned_at",
            "updated_at",
        ],
    ),
    (
        "occupancy_mirror",
        "PRAGMA table_info(occupancy_mirror)",
        &[
            "occupancy_lease_id",
            "repository_binding_id",
            "user_id",
            "state",
            "fencing_token",
            "acquired_at",
            "expires_at",
            "updated_at",
        ],
    ),
    (
        "worker_process_registry",
        "PRAGMA table_info(worker_process_registry)",
        &[
            "worker_session_id",
            "worker_id",
            "worker_instance_id",
            "pid",
            "process_start_identity",
            "repository_binding_id",
            "occupancy_lease_id",
            "launch_grant_id",
            "data_directory",
            "state",
            "last_observed_at",
        ],
    ),
    (
        "worker_launch_receipts",
        "PRAGMA table_info(worker_launch_receipts)",
        &[
            "launch_grant_id",
            "worker_session_id",
            "ack_status",
            "idempotency_key",
            "receipt_payload",
            "received_at",
        ],
    ),
    (
        "candidate_local_refs",
        "PRAGMA table_info(candidate_local_refs)",
        &[
            "candidate_id",
            "worker_session_id",
            "repository_binding_id",
            "local_git_ref",
            "local_state",
            "created_at",
        ],
    ),
    (
        "client_outbox",
        "PRAGMA table_info(client_outbox)",
        &[
            "outbox_sequence",
            "message_id",
            "client_node_id",
            "client_instance_id",
            "envelope_sequence",
            "kind",
            "payload",
            "occurred_at",
            "published",
        ],
    ),
    (
        "client_inbox_cursor",
        "PRAGMA table_info(client_inbox_cursor)",
        &[
            "server_profile_id",
            "last_sequence",
            "last_message_id",
            "updated_at",
        ],
    ),
    (
        "connect_code_state",
        "PRAGMA table_info(connect_code_state)",
        &[
            "singleton",
            "connect_code_id",
            "code_digest",
            "generation",
            "issued_by_instance_id",
            "expires_at",
            "state",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "client_connection_policy",
        "PRAGMA table_info(client_connection_policy)",
        &[
            "singleton",
            "accepting_connections",
            "lock_state",
            "updated_at",
        ],
    ),
];

fn validate_store_schema(transaction: &rusqlite::Transaction<'_>) -> Result<(), DeviceStoreError> {
    for (table, pragma, expected_columns) in STORE_SCHEMA_COLUMNS {
        let columns = table_columns(transaction, pragma)?;
        if columns != *expected_columns {
            return Err(DeviceStoreError::adapter(format!(
                "device client table {table} schema is not canonical"
            )));
        }
    }
    Ok(())
}

fn table_columns(
    transaction: &rusqlite::Transaction<'_>,
    pragma: &str,
) -> Result<Vec<String>, DeviceStoreError> {
    let mut statement = transaction.prepare(pragma).map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sql_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
}

fn sqlite_open_lock(
    database_path: &Path,
    open_deadline: StdInstant,
) -> Result<Arc<Mutex<()>>, DeviceStoreError> {
    let locks = SQLITE_OPEN_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = acquire_mutex_before_open_deadline(locks, open_deadline)?;
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(database_path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(database_path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn acquire_mutex_before_open_deadline<T>(
    mutex: &Mutex<T>,
    open_deadline: StdInstant,
) -> Result<MutexGuard<'_, T>, DeviceStoreError> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(TryLockError::WouldBlock) => {
                let remaining = remaining_sqlite_open_time(open_deadline)?;
                thread::sleep(remaining.min(SQLITE_OPEN_RETRY_INTERVAL));
            }
        }
    }
}

fn set_open_busy_deadline(
    connection: &Connection,
    open_deadline: StdInstant,
) -> Result<(), DeviceStoreError> {
    remaining_sqlite_open_time(open_deadline)?;
    SQLITE_OPEN_BUSY_DEADLINE.set(Some(open_deadline));
    connection
        .busy_handler(Some(sqlite_open_busy_handler))
        .map_err(sql_error)
}

fn sqlite_open_busy_handler(_prior_calls: i32) -> bool {
    SQLITE_OPEN_BUSY_DEADLINE.get().is_some_and(|deadline| {
        let Some(remaining) = deadline.checked_duration_since(StdInstant::now()) else {
            return false;
        };
        thread::sleep(remaining.min(SQLITE_OPEN_RETRY_INTERVAL));
        true
    })
}

fn remaining_sqlite_open_time(open_deadline: StdInstant) -> Result<Duration, DeviceStoreError> {
    open_deadline
        .checked_duration_since(StdInstant::now())
        .filter(|remaining| *remaining >= Duration::from_millis(1))
        .ok_or_else(|| {
            DeviceStoreError::adapter("SQLite device client open exceeded its five-second limit")
        })
}
