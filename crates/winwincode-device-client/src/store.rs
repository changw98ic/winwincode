// SPDX-License-Identifier: Apache-2.0

//! Local device-client `SQLite` store holding the eleven local tables from
//! plan section 8.
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

use crate::StoreOutbox;

/// Current schema version of the local device-client database.
pub const CLIENT_STORE_SCHEMA_VERSION: i64 = 1;

const DATABASE_FILE_NAME: &str = "device-client.sqlite3";
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(1);
/// Largest integer exactly representable where it matters for wire cursors;
/// matches the durable-cursor bound used by `winwincode-storage`.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 200;
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

/// One durable server-to-client replay cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientInboxCursor {
    pub server_profile_id: String,
    pub last_sequence: u64,
    pub last_message_id: Option<String>,
    pub updated_at: String,
}

/// Local `SQLite` implementation of the device-client store.
pub struct DeviceStore {
    connection: Option<Connection>,
    read_connection: Mutex<Connection>,
    database_path: PathBuf,
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
        envelope: &winwincode_client_port::messages::ClientToServerEnvelope,
        kind: &str,
    ) -> Result<u64, DeviceStoreError> {
        if envelope.schema_version
            != winwincode_client_port::messages::CLIENT_CONTROL_PORT_SCHEMA_VERSION
        {
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
}

/// COMPATIBILITY NOTE (awaiting alignment): implements the local outbox
/// seam declared in [`crate`] until the canonical outbox trait from the
/// `winwincode-client-port` lane lands; see the trait documentation.
impl StoreOutbox for DeviceStore {
    fn append_outbox_envelope(
        &mut self,
        envelope: &winwincode_client_port::messages::ClientToServerEnvelope,
        kind: &str,
    ) -> Result<u64, DeviceStoreError> {
        DeviceStore::append_outbox_envelope(self, envelope, kind)
    }

    fn pending_outbox_envelopes(&self) -> Result<Vec<ClientOutboxEntry>, DeviceStoreError> {
        DeviceStore::pending_outbox_envelopes(self)
    }

    fn mark_outbox_published(&mut self, message_id: &str) -> Result<(), DeviceStoreError> {
        DeviceStore::mark_outbox_published(self, message_id)
    }
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

const STORE_SCHEMA: &str = r"
CREATE TABLE device_identity (
    device_id TEXT PRIMARY KEY NOT NULL,
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
