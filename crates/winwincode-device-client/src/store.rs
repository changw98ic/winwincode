// SPDX-License-Identifier: Apache-2.0

//! The open sequence, transaction discipline, and the

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError, Weak};
use std::thread;
use std::time::{Duration, Instant as StdInstant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use winwincode_client_port::domain::{
    ClientLockState, ClientOccupancyReleaseMode, ClientWorkerStopReason, ConnectCodeState,
    RepositoryAvailability, RepositoryDirtyState,
};
use winwincode_client_port::exchange::{
    CompactingOutbox, FrameCodec, FrameOutbox, OutboxSnapshot, StoredFrame,
};
use winwincode_client_port::messages::{
    CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientToServerEnvelope, ClientToServerMessage,
};

/// Schema identity of the current local device-client database.
pub const CLIENT_STORE_SCHEMA_VERSION: i64 = 9;

const DATABASE_FILE_NAME: &str = "device-client.sqlite3";
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(1);
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 200;
const MAX_URL_BYTES: usize = 2048;
const MAX_PATH_BYTES: usize = 4096;

static SQLITE_OPEN_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
thread_local! {
    static SQLITE_OPEN_BUSY_DEADLINE: Cell<Option<StdInstant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceStoreErrorKind {
    InvalidInput,
    Conflict,
    NotFound,
    Closed,
    Adapter,
}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathMappingRecord {
    pub repository_binding_id: String,
    pub canonical_path: String,
    pub git_common_directory: Option<String>,
    pub last_canonicalized_at: Option<String>,
    pub local_state: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryLocalStateRecord {
    pub repository_binding_id: String,
    pub dirty_state: RepositoryDirtyState,
    pub availability: RepositoryAvailability,
    pub head_commit: Option<String>,
    pub last_scanned_at: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientOutboxEntry {
    pub outbox_sequence: u64,
    pub message_id: String,
    pub client_node_id: String,
    pub client_instance_id: String,
    pub envelope_sequence: u64,
    pub kind: String,
    pub payload: Vec<u8>,
    pub occurred_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientInboxCursorUpdate {
    pub server_profile_id: String,
    pub last_sequence: u64,
    pub last_message_id: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectCodeStateRecord {
    pub connect_code_id: String,
    pub code_digest: String,
    pub generation: u64,
    pub issued_by_instance_id: String,
    pub expires_at: String,
    pub state: ConnectCodeState,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionPolicyRecord {
    pub accepting_connections: bool,
    pub lock_state: ClientLockState,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientInboxCursor {
    pub server_profile_id: String,
    pub last_sequence: u64,
    pub last_message_id: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OccupancyMirrorRecord {
    pub occupancy_lease_id: String,
    pub fencing_token: u64,
    pub holder_user_id: Option<String>,
    pub mirror_revision: u64,
    pub claim_request_id: Option<String>,
    pub idle_expires_at: Option<String>,
    pub acknowledged_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OccupancyMirrorUpdate {
    pub occupancy_lease_id: String,
    pub fencing_token: u64,
    pub holder_user_id: Option<String>,
    pub claim_request_id: Option<String>,
    pub idle_expires_at: Option<String>,
    pub acknowledged_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OccupancyMirrorAdvance {
    Advanced(OccupancyMirrorRecord),
    Unchanged(OccupancyMirrorRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OccupancyReleaseIntentRecord {
    pub idempotency_key: String,
    pub command_message_id: String,
    pub occupancy_lease_id: String,
    pub fencing_token: u64,
    pub mode: ClientOccupancyReleaseMode,
    pub affected_worker_sessions: u64,
    pub recorded_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OccupancyReleaseIntentOutcome {
    Recorded(OccupancyReleaseIntentRecord),
    Duplicate(OccupancyReleaseIntentRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerStopIntentRecord {
    pub command_message_id: String,
    pub idempotency_key: String,
    pub worker_session_id: String,
    pub worker_id: String,
    pub reason: ClientWorkerStopReason,
    pub occupancy_lease_id: String,
    pub fencing_token: u64,
    pub expected_revision: u64,
    pub receipt_payload: Option<Vec<u8>>,
    pub recorded_at: String,
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkerStopIntentOutcome {
    Recorded(WorkerStopIntentRecord),
    Duplicate(WorkerStopIntentRecord),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerProcessRecord {
    pub worker_session_id: String,
    pub worker_id: String,
    pub worker_instance_id: String,
    pub pid: u32,
    pub process_start_identity: String,
    pub repository_binding_id: String,
    pub occupancy_lease_id: String,
    pub launch_grant_id: String,
    pub data_directory: String,
    pub state: String,
    pub exit_code: Option<i64>,
    pub last_observed_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OutboxStream {
    client_node_id: String,
    client_instance_id: String,
}

pub struct DeviceStore {
    connection: Option<Connection>,
    read_connection: Mutex<Connection>,
    database_path: PathBuf,
    outbox_stream: Option<OutboxStream>,
}

impl DeviceStore {
    pub fn record_command_result(
        &mut self,
        result: &winwincode_client_port::messages::ClientCommandAckPayload,
    ) -> Result<(), DeviceStoreError> {
        let payload = serde_json::to_vec(result)
            .map_err(|_| DeviceStoreError::invalid("invalid command receipt"))?;
        self.connection()?
            .execute(
                "INSERT INTO server_command_results (command_message_id, payload) VALUES (?1, ?2)
             ON CONFLICT(command_message_id) DO UPDATE SET payload = excluded.payload",
                params![result.command_message_id, payload],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn command_result(
        &self,
        message_id: &str,
    ) -> Result<Option<winwincode_client_port::messages::ClientCommandAckPayload>, DeviceStoreError>
    {
        let payload: Option<Vec<u8>> = self
            .connection()?
            .query_row(
                "SELECT payload FROM server_command_results WHERE command_message_id = ?1",
                [message_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        payload
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|_| DeviceStoreError::adapter("invalid stored command receipt"))
            })
            .transpose()
    }
    /// Opens the local database and initializes or validates the current schema.
    /// durability settings, or current schema cannot be prepared.
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
        let open_lock = sqlite_open_lock(&database_path, open_deadline)?;
        let _open_guard = acquire_mutex_before_open_deadline(&open_lock, open_deadline)?;
        // The daemon and CLI are separate processes. Journal-mode changes and
        // schema preparation must share the same initialization lock across them.
        let _process_open_guard = acquire_sqlite_process_open_lock(&database_path, open_deadline)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
        let mut connection =
            Connection::open_with_flags(&database_path, flags).map_err(sql_error)?;
        #[cfg(test)]
        open_process_concurrency_tests::after_sqlite_connection_opened();
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
        #[cfg(test)]
        worker_report_schema_tests::before_schema_prepare_for_test(open_deadline);
        set_open_busy_deadline(&connection, open_deadline)?;
        initialize_schema(&mut connection)?;

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

    pub fn put_repository_local_state(
        &mut self,
        record: &RepositoryLocalStateRecord,
    ) -> Result<(), DeviceStoreError> {
        require_non_empty(
            &record.repository_binding_id,
            "repository binding id",
            MAX_ID_BYTES,
        )?;
        require_non_empty(
            &record.updated_at,
            "repository state updated at",
            MAX_ID_BYTES,
        )?;
        let connection = self.connection_mut()?;
        connection
            .execute(
                "INSERT INTO repository_local_state \
                 (repository_binding_id, dirty_state, availability, head_commit, \
                  last_scanned_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (repository_binding_id) DO UPDATE SET \
                 dirty_state = excluded.dirty_state, \
                 availability = excluded.availability, \
                 head_commit = excluded.head_commit, \
                 last_scanned_at = excluded.last_scanned_at, \
                 updated_at = excluded.updated_at",
                params![
                    record.repository_binding_id,
                    dirty_state_wire_name(record.dirty_state),
                    availability_wire_name(record.availability),
                    record.head_commit,
                    record.last_scanned_at,
                    record.updated_at,
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn repository_local_state(
        &self,
        repository_binding_id: &str,
    ) -> Result<Option<RepositoryLocalStateRecord>, DeviceStoreError> {
        self.connection()?
            .query_row(
                "SELECT repository_binding_id, dirty_state, availability, head_commit, \
                 last_scanned_at, updated_at \
                 FROM repository_local_state WHERE repository_binding_id = ?1",
                params![repository_binding_id],
                row_to_repository_local_state,
            )
            .optional()
            .map_err(sql_error)
    }

    pub fn repository_local_states(
        &self,
    ) -> Result<Vec<RepositoryLocalStateRecord>, DeviceStoreError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT repository_binding_id, dirty_state, availability, head_commit, \
                 last_scanned_at, updated_at \
                 FROM repository_local_state ORDER BY repository_binding_id",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], row_to_repository_local_state)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    pub fn delete_repository_binding(
        &mut self,
        repository_binding_id: &str,
    ) -> Result<bool, DeviceStoreError> {
        require_non_empty(repository_binding_id, "repository binding id", MAX_ID_BYTES)?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        transaction
            .execute(
                "DELETE FROM repository_local_state WHERE repository_binding_id = ?1",
                params![repository_binding_id],
            )
            .map_err(sql_error)?;
        let changed = transaction
            .execute(
                "DELETE FROM repository_path_mapping WHERE repository_binding_id = ?1",
                params![repository_binding_id],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(changed > 0)
    }

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

    pub fn occupancy_mirror(&self) -> Result<Option<OccupancyMirrorRecord>, DeviceStoreError> {
        let connection = self.connection()?;
        read_occupancy_mirror(connection)
    }

    pub fn advance_occupancy_mirror(
        &mut self,
        update: &OccupancyMirrorUpdate,
    ) -> Result<OccupancyMirrorAdvance, DeviceStoreError> {
        validate_mirror_update(update)?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let advance = if let Some(stored) = read_occupancy_mirror(&transaction)? {
            if stored.occupancy_lease_id == update.occupancy_lease_id
                && stored.fencing_token == update.fencing_token
            {
                if stored.holder_user_id != update.holder_user_id {
                    return Err(DeviceStoreError::conflict(format!(
                        "occupancy mirror update for lease {} token {} names a \
                         different holder than the stored mirror",
                        update.occupancy_lease_id, update.fencing_token
                    )));
                }
                OccupancyMirrorAdvance::Unchanged(stored)
            } else if update.fencing_token <= stored.fencing_token {
                return Err(DeviceStoreError::conflict(format!(
                    "occupancy mirror token {} does not advance past the \
                     stored token {}",
                    update.fencing_token, stored.fencing_token
                )));
            } else {
                let record = OccupancyMirrorRecord {
                    mirror_revision: stored.mirror_revision + 1,
                    ..mirror_record_from_update(update)
                };
                write_occupancy_mirror(&transaction, &record)?;
                OccupancyMirrorAdvance::Advanced(record)
            }
        } else {
            let record = OccupancyMirrorRecord {
                mirror_revision: 1,
                ..mirror_record_from_update(update)
            };
            write_occupancy_mirror(&transaction, &record)?;
            OccupancyMirrorAdvance::Advanced(record)
        };
        transaction.commit().map_err(sql_error)?;
        Ok(advance)
    }

    pub fn count_lease_worker_sessions(
        &self,
        occupancy_lease_id: &str,
    ) -> Result<u64, DeviceStoreError> {
        require_non_empty(occupancy_lease_id, "occupancy lease id", MAX_ID_BYTES)?;
        let count: i64 = self
            .connection()?
            .query_row(
                "SELECT COUNT(*) FROM worker_process_registry \
                 WHERE occupancy_lease_id = ?1",
                params![occupancy_lease_id],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        u64::try_from(count).map_err(|_| DeviceStoreError::adapter("worker count is negative"))
    }

    pub fn put_worker_process(
        &mut self,
        record: &WorkerProcessRecord,
    ) -> Result<(), DeviceStoreError> {
        validate_worker_process_record(record)?;
        let connection = self.connection_mut()?;
        connection
            .execute(
                "INSERT INTO worker_process_registry \
                 (worker_session_id, worker_id, worker_instance_id, pid, \
                  process_start_identity, repository_binding_id, occupancy_lease_id, \
                  launch_grant_id, data_directory, state, exit_code, last_observed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                 ON CONFLICT (worker_session_id) DO UPDATE SET \
                 worker_id = excluded.worker_id, \
                 worker_instance_id = excluded.worker_instance_id, \
                 pid = excluded.pid, \
                 process_start_identity = excluded.process_start_identity, \
                 repository_binding_id = excluded.repository_binding_id, \
                 occupancy_lease_id = excluded.occupancy_lease_id, \
                 launch_grant_id = excluded.launch_grant_id, \
                 data_directory = excluded.data_directory, \
                 state = excluded.state, \
                 exit_code = excluded.exit_code, \
                 last_observed_at = excluded.last_observed_at",
                params![
                    record.worker_session_id,
                    record.worker_id,
                    record.worker_instance_id,
                    i64::from(record.pid),
                    record.process_start_identity,
                    record.repository_binding_id,
                    record.occupancy_lease_id,
                    record.launch_grant_id,
                    record.data_directory,
                    record.state,
                    record.exit_code,
                    record.last_observed_at,
                ],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn worker_process(
        &self,
        worker_session_id: &str,
    ) -> Result<Option<WorkerProcessRecord>, DeviceStoreError> {
        require_non_empty(worker_session_id, "worker session id", MAX_ID_BYTES)?;
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT worker_session_id, worker_id, worker_instance_id, pid, \
                 process_start_identity, repository_binding_id, occupancy_lease_id, \
                 launch_grant_id, data_directory, state, exit_code, last_observed_at \
                 FROM worker_process_registry WHERE worker_session_id = ?1",
            )
            .map_err(sql_error)?;
        statement
            .query_row(params![worker_session_id], row_to_worker_process)
            .optional()
            .map_err(sql_error)
    }

    pub fn worker_processes(&self) -> Result<Vec<WorkerProcessRecord>, DeviceStoreError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT worker_session_id, worker_id, worker_instance_id, pid, \
                 process_start_identity, repository_binding_id, occupancy_lease_id, \
                 launch_grant_id, data_directory, state, exit_code, last_observed_at \
                 FROM worker_process_registry ORDER BY worker_session_id",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], row_to_worker_process)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    pub fn worker_report_candidates(
        &self,
        family: &str,
        lease: Option<&str>,
    ) -> Result<Vec<WorkerProcessRecord>, DeviceStoreError> {
        if !matches!(family, "reconcile" | "exit") {
            return Err(DeviceStoreError::adapter("unknown worker report family"));
        }
        let active = if family == "reconcile" {
            "SELECT worker_session_id FROM worker_process_registry WHERE state NOT IN ('exited','crashed','missing') AND (?2 IS NULL OR occupancy_lease_id=?2) UNION "
        } else {
            ""
        };
        let sql = format!(
            "SELECT worker_session_id,worker_id,worker_instance_id,pid,process_start_identity,repository_binding_id,occupancy_lease_id,launch_grant_id,data_directory,state,exit_code,last_observed_at FROM worker_process_registry WHERE worker_session_id IN ({active}SELECT worker_session_id FROM worker_report_pending WHERE family=?1) AND (?2 IS NULL OR occupancy_lease_id=?2) ORDER BY worker_session_id"
        );
        let mut stmt = self.connection()?.prepare(&sql).map_err(sql_error)?;
        stmt.query_map(params![family, lease], row_to_worker_process)
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)
    }

    pub fn worker_report_queued(
        &mut self,
        family: &str,
        record: &WorkerProcessRecord,
    ) -> Result<(), DeviceStoreError> {
        if !matches!(family, "reconcile" | "exit") {
            return Err(DeviceStoreError::adapter("unknown worker report family"));
        }
        self.connection_mut()?.execute("DELETE FROM worker_report_pending WHERE family=?1 AND worker_session_id=?2 AND EXISTS (
            SELECT 1 FROM worker_process_registry WHERE worker_session_id=?2 AND worker_instance_id=?3 AND pid=?4 AND process_start_identity=?5 AND occupancy_lease_id=?6 AND state=?7 AND exit_code IS ?8)",
            params![family, record.worker_session_id, record.worker_instance_id, i64::from(record.pid), record.process_start_identity, record.occupancy_lease_id, record.state, record.exit_code]).map_err(sql_error)?;
        Ok(())
    }

    pub fn has_running_worker_for_lease(&self, lease: &str) -> Result<bool, DeviceStoreError> {
        require_non_empty(lease, "occupancy lease id", MAX_ID_BYTES)?;
        self.connection()?.query_row("SELECT EXISTS(SELECT 1 FROM worker_process_registry WHERE state='running' AND occupancy_lease_id=?1)", [lease], |row| row.get(0)).map_err(sql_error)
    }

    pub fn worker_processes_for_lease(
        &self,
        occupancy_lease_id: &str,
    ) -> Result<Vec<WorkerProcessRecord>, DeviceStoreError> {
        require_non_empty(occupancy_lease_id, "occupancy lease id", MAX_ID_BYTES)?;
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT worker_session_id, worker_id, worker_instance_id, pid, \
                 process_start_identity, repository_binding_id, occupancy_lease_id, \
                 launch_grant_id, data_directory, state, exit_code, last_observed_at \
                 FROM worker_process_registry WHERE occupancy_lease_id = ?1 \
                 ORDER BY worker_session_id",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(params![occupancy_lease_id], row_to_worker_process)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    pub fn count_worker_processes_in_state(&self, state: &str) -> Result<u64, DeviceStoreError> {
        require_non_empty(state, "worker state", MAX_ID_BYTES)?;
        let count: i64 = self
            .connection()?
            .query_row(
                "SELECT COUNT(*) FROM worker_process_registry WHERE state = ?1",
                params![state],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        u64::try_from(count).map_err(|_| DeviceStoreError::adapter("worker count is negative"))
    }

    pub fn mark_worker_process_state(
        &mut self,
        worker_session_id: &str,
        state: &str,
        exit_code: Option<i64>,
        observed_at: &str,
    ) -> Result<bool, DeviceStoreError> {
        require_non_empty(worker_session_id, "worker session id", MAX_ID_BYTES)?;
        require_non_empty(state, "worker state", MAX_ID_BYTES)?;
        require_non_empty(observed_at, "observed at", MAX_ID_BYTES)?;
        let changed = self
            .connection_mut()?
            .execute(
                "UPDATE worker_process_registry \
                 SET state = ?2, exit_code = ?3, last_observed_at = ?4 \
                 WHERE worker_session_id = ?1",
                params![worker_session_id, state, exit_code, observed_at],
            )
            .map_err(sql_error)?;
        Ok(changed > 0)
    }

    pub fn record_occupancy_release_intent(
        &mut self,
        intent: &OccupancyReleaseIntentRecord,
    ) -> Result<OccupancyReleaseIntentOutcome, DeviceStoreError> {
        validate_release_intent(intent)?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(stored) = read_release_intent(&transaction, &intent.idempotency_key)? {
            transaction.commit().map_err(sql_error)?;
            return Ok(OccupancyReleaseIntentOutcome::Duplicate(stored));
        }
        transaction
            .execute(
                "INSERT INTO occupancy_release_intents \
                 (idempotency_key, command_message_id, occupancy_lease_id, \
                  fencing_token, mode, affected_worker_sessions, recorded_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    intent.idempotency_key,
                    intent.command_message_id,
                    intent.occupancy_lease_id,
                    i64::try_from(intent.fencing_token).map_err(|_| {
                        DeviceStoreError::invalid("release fencing token is out of range")
                    })?,
                    release_mode_to_str(intent.mode),
                    i64::try_from(intent.affected_worker_sessions).map_err(|_| {
                        DeviceStoreError::invalid("affected worker count is out of range")
                    })?,
                    intent.recorded_at,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(OccupancyReleaseIntentOutcome::Recorded(intent.clone()))
    }

    pub fn occupancy_release_intent(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<OccupancyReleaseIntentRecord>, DeviceStoreError> {
        require_non_empty(idempotency_key, "idempotency key", MAX_ID_BYTES)?;
        let connection = self.connection()?;
        read_release_intent(connection, idempotency_key)
    }

    pub fn occupancy_release_intents(
        &self,
    ) -> Result<Vec<OccupancyReleaseIntentRecord>, DeviceStoreError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT idempotency_key, command_message_id, occupancy_lease_id, \
                 fencing_token, mode, affected_worker_sessions, recorded_at \
                 FROM occupancy_release_intents ORDER BY rowid",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], row_to_release_intent)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    pub fn record_worker_stop_intent(
        &mut self,
        intent: &WorkerStopIntentRecord,
    ) -> Result<WorkerStopIntentOutcome, DeviceStoreError> {
        validate_worker_stop_intent(intent)?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let existing = read_worker_stop_intent(&transaction, &intent.command_message_id)?;
        if let Some(stored) = existing {
            if !same_worker_stop_intent(&stored, intent) {
                return Err(DeviceStoreError::conflict(
                    "worker stop command identity changed during replay",
                ));
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerStopIntentOutcome::Duplicate(stored));
        }
        if let Some(stored) =
            read_worker_stop_intent_by_idempotency(&transaction, &intent.idempotency_key)?
        {
            if !same_worker_stop_intent(&stored, intent) {
                return Err(DeviceStoreError::conflict(
                    "worker stop idempotency key is bound to another command",
                ));
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(WorkerStopIntentOutcome::Duplicate(stored));
        }
        transaction
            .execute(
                "INSERT INTO worker_stop_intents \
                 (command_message_id, idempotency_key, worker_session_id, worker_id, reason, \
                  occupancy_lease_id, fencing_token, expected_revision, receipt_payload, \
                  recorded_at, completed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, NULL)",
                params![
                    intent.command_message_id,
                    intent.idempotency_key,
                    intent.worker_session_id,
                    intent.worker_id,
                    worker_stop_reason_wire_name(intent.reason),
                    intent.occupancy_lease_id,
                    i64::try_from(intent.fencing_token).map_err(|_| {
                        DeviceStoreError::invalid("worker stop fencing token is out of range")
                    })?,
                    i64::try_from(intent.expected_revision).map_err(|_| {
                        DeviceStoreError::invalid("worker stop revision is out of range")
                    })?,
                    intent.recorded_at,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(WorkerStopIntentOutcome::Recorded(intent.clone()))
    }

    pub fn worker_stop_intent(
        &self,
        command_message_id: &str,
    ) -> Result<Option<WorkerStopIntentRecord>, DeviceStoreError> {
        require_non_empty(
            command_message_id,
            "worker stop command message id",
            MAX_ID_BYTES,
        )?;
        read_worker_stop_intent(self.connection()?, command_message_id)
    }

    pub fn worker_stop_intent_by_idempotency(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<WorkerStopIntentRecord>, DeviceStoreError> {
        require_non_empty(idempotency_key, "worker stop idempotency key", MAX_ID_BYTES)?;
        read_worker_stop_intent_by_idempotency(self.connection()?, idempotency_key)
    }

    pub fn pending_worker_stop_intents(
        &self,
    ) -> Result<Vec<WorkerStopIntentRecord>, DeviceStoreError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT command_message_id, idempotency_key, worker_session_id, worker_id, \
             reason, occupancy_lease_id, fencing_token, expected_revision, receipt_payload, \
             recorded_at, completed_at FROM worker_stop_intents \
             WHERE receipt_payload IS NULL ORDER BY recorded_at, rowid",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], row_to_worker_stop_intent)
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    pub fn complete_worker_stop_intent(
        &mut self,
        command_message_id: &str,
        receipt_payload: &[u8],
        completed_at: &str,
    ) -> Result<(), DeviceStoreError> {
        require_non_empty(
            command_message_id,
            "worker stop command message id",
            MAX_ID_BYTES,
        )?;
        if receipt_payload.is_empty() || receipt_payload.len() > 64 * 1024 {
            return Err(DeviceStoreError::invalid(
                "worker stop receipt payload must be between 1 and 65536 bytes",
            ));
        }
        require_non_empty(completed_at, "worker stop completed at", MAX_ID_BYTES)?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let Some(stored) = read_worker_stop_intent(&transaction, command_message_id)? else {
            return Err(DeviceStoreError::not_found(format!(
                "worker stop command {command_message_id} does not have a durable intent"
            )));
        };
        if let Some(existing) = stored.receipt_payload {
            if existing != receipt_payload {
                return Err(DeviceStoreError::conflict(
                    "worker stop receipt changed during replay",
                ));
            }
        } else {
            transaction
                .execute(
                    "UPDATE worker_stop_intents SET receipt_payload = ?2, completed_at = ?3 \
                     WHERE command_message_id = ?1",
                    params![command_message_id, receipt_payload, completed_at],
                )
                .map_err(sql_error)?;
        }
        transaction.commit().map_err(sql_error)
    }
}

impl DeviceStore {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerProfileRecord {
    pub server_profile_id: String,
    pub base_url: String,
    pub display_name: String,
    pub created_at: String,
    pub last_connected_at: Option<String>,
}

impl FrameOutbox for DeviceStore {
    type Error = DeviceStoreError;

    fn load(&mut self) -> Result<Option<OutboxSnapshot>, Self::Error> {
        let stream = self.outbox_stream()?.clone();
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(sql_error)?;
        let (ack_sequence, highest_sequence) =
            outbox_stream_cursor(&transaction, &stream.client_node_id)?;
        let Some(highest_sequence) = highest_sequence else {
            transaction.commit().map_err(sql_error)?;
            return Ok(None);
        };
        let mut statement = transaction
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
        drop(statement);
        transaction.commit().map_err(sql_error)?;
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

fn i64_prefix(sequence: u64) -> Result<i64, DeviceStoreError> {
    i64::try_from(sequence).map_err(|_| {
        DeviceStoreError::adapter("outbox sequence is outside the SQLite integer range")
    })
}

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

pub(crate) fn envelope_kind(message: &ClientToServerMessage) -> Result<String, DeviceStoreError> {
    let value = serde_json::to_value(message)
        .map_err(|error| DeviceStoreError::invalid(format!("message is not encodable: {error}")))?;
    value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| DeviceStoreError::invalid("encoded message carries no kind string"))
}

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

fn row_to_repository_local_state(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<RepositoryLocalStateRecord> {
    Ok(RepositoryLocalStateRecord {
        repository_binding_id: row.get(0)?,
        dirty_state: parse_dirty_state_wire_name(&row.get::<_, String>(1)?)?,
        availability: parse_availability_wire_name(&row.get::<_, String>(2)?)?,
        head_commit: row.get(3)?,
        last_scanned_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

#[must_use]
pub const fn availability_wire_name(availability: RepositoryAvailability) -> &'static str {
    match availability {
        RepositoryAvailability::Available => "available",
        RepositoryAvailability::Dirty => "dirty",
        RepositoryAvailability::Unavailable => "unavailable",
        RepositoryAvailability::Moved => "moved",
        RepositoryAvailability::InvalidGit => "invalid_git",
        RepositoryAvailability::PermissionDenied => "permission_denied",
        RepositoryAvailability::ScanFailed => "scan_failed",
    }
}

fn parse_availability_wire_name(value: &str) -> rusqlite::Result<RepositoryAvailability> {
    match value {
        "available" => Ok(RepositoryAvailability::Available),
        "dirty" => Ok(RepositoryAvailability::Dirty),
        "unavailable" => Ok(RepositoryAvailability::Unavailable),
        "moved" => Ok(RepositoryAvailability::Moved),
        "invalid_git" => Ok(RepositoryAvailability::InvalidGit),
        "permission_denied" => Ok(RepositoryAvailability::PermissionDenied),
        "scan_failed" => Ok(RepositoryAvailability::ScanFailed),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            format!("stored availability {other} is not in the plan 13.5 vocabulary").into(),
        )),
    }
}

#[must_use]
pub const fn dirty_state_wire_name(dirty_state: RepositoryDirtyState) -> &'static str {
    match dirty_state {
        RepositoryDirtyState::Clean => "clean",
        RepositoryDirtyState::Dirty => "dirty",
    }
}

fn parse_dirty_state_wire_name(value: &str) -> rusqlite::Result<RepositoryDirtyState> {
    match value {
        "clean" => Ok(RepositoryDirtyState::Clean),
        "dirty" => Ok(RepositoryDirtyState::Dirty),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("stored dirty state {other} is not clean or dirty").into(),
        )),
    }
}

#[must_use]
const fn worker_stop_reason_wire_name(reason: ClientWorkerStopReason) -> &'static str {
    match reason {
        ClientWorkerStopReason::OccupantRequested => "occupant_requested",
        ClientWorkerStopReason::DrainingComplete => "draining_complete",
        ClientWorkerStopReason::LeaseRecovered => "lease_recovered",
        ClientWorkerStopReason::GrantRevoked => "grant_revoked",
        ClientWorkerStopReason::Superseded => "superseded",
    }
}

fn parse_worker_stop_reason_wire_name(value: &str) -> rusqlite::Result<ClientWorkerStopReason> {
    match value {
        "occupant_requested" => Ok(ClientWorkerStopReason::OccupantRequested),
        "draining_complete" => Ok(ClientWorkerStopReason::DrainingComplete),
        "lease_recovered" => Ok(ClientWorkerStopReason::LeaseRecovered),
        "grant_revoked" => Ok(ClientWorkerStopReason::GrantRevoked),
        "superseded" => Ok(ClientWorkerStopReason::Superseded),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            format!("stored worker stop reason {other} is not in the vocabulary").into(),
        )),
    }
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

fn read_occupancy_mirror(
    connection: &Connection,
) -> Result<Option<OccupancyMirrorRecord>, DeviceStoreError> {
    connection
        .query_row(
            "SELECT occupancy_lease_id, fencing_token, holder_user_id, \
             mirror_revision, claim_request_id, idle_expires_at, \
             acknowledged_at, updated_at \
             FROM occupancy_mirror WHERE singleton = 1",
            [],
            row_to_occupancy_mirror,
        )
        .optional()
        .map_err(sql_error)
}

fn write_occupancy_mirror(
    transaction: &rusqlite::Transaction<'_>,
    record: &OccupancyMirrorRecord,
) -> Result<(), DeviceStoreError> {
    transaction
        .execute(
            "INSERT INTO occupancy_mirror \
             (singleton, occupancy_lease_id, fencing_token, holder_user_id, \
              mirror_revision, claim_request_id, idle_expires_at, \
              acknowledged_at, updated_at) \
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (singleton) DO UPDATE SET \
             occupancy_lease_id = excluded.occupancy_lease_id, \
             fencing_token = excluded.fencing_token, \
             holder_user_id = excluded.holder_user_id, \
             mirror_revision = excluded.mirror_revision, \
             claim_request_id = excluded.claim_request_id, \
             idle_expires_at = excluded.idle_expires_at, \
             acknowledged_at = excluded.acknowledged_at, \
             updated_at = excluded.updated_at",
            params![
                record.occupancy_lease_id,
                i64::try_from(record.fencing_token).map_err(|_| DeviceStoreError::invalid(
                    "occupancy fencing token is outside the SQLite integer range"
                ))?,
                record.holder_user_id,
                i64::try_from(record.mirror_revision).map_err(|_| DeviceStoreError::invalid(
                    "occupancy mirror revision is outside the SQLite integer range"
                ))?,
                record.claim_request_id,
                record.idle_expires_at,
                record.acknowledged_at,
                record.updated_at,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn mirror_record_from_update(update: &OccupancyMirrorUpdate) -> OccupancyMirrorRecord {
    OccupancyMirrorRecord {
        occupancy_lease_id: update.occupancy_lease_id.clone(),
        fencing_token: update.fencing_token,
        holder_user_id: update.holder_user_id.clone(),
        mirror_revision: 1,
        claim_request_id: update.claim_request_id.clone(),
        idle_expires_at: update.idle_expires_at.clone(),
        acknowledged_at: update.acknowledged_at.clone(),
        updated_at: update.acknowledged_at.clone(),
    }
}

fn read_release_intent(
    connection: &Connection,
    idempotency_key: &str,
) -> Result<Option<OccupancyReleaseIntentRecord>, DeviceStoreError> {
    connection
        .query_row(
            "SELECT idempotency_key, command_message_id, occupancy_lease_id, \
             fencing_token, mode, affected_worker_sessions, recorded_at \
             FROM occupancy_release_intents WHERE idempotency_key = ?1",
            params![idempotency_key],
            row_to_release_intent,
        )
        .optional()
        .map_err(sql_error)
}

fn read_worker_stop_intent(
    connection: &Connection,
    command_message_id: &str,
) -> Result<Option<WorkerStopIntentRecord>, DeviceStoreError> {
    connection
        .query_row(
            "SELECT command_message_id, idempotency_key, worker_session_id, worker_id, \
             reason, occupancy_lease_id, fencing_token, expected_revision, receipt_payload, \
             recorded_at, completed_at FROM worker_stop_intents \
             WHERE command_message_id = ?1",
            params![command_message_id],
            row_to_worker_stop_intent,
        )
        .optional()
        .map_err(sql_error)
}

fn read_worker_stop_intent_by_idempotency(
    connection: &Connection,
    idempotency_key: &str,
) -> Result<Option<WorkerStopIntentRecord>, DeviceStoreError> {
    connection
        .query_row(
            "SELECT command_message_id, idempotency_key, worker_session_id, worker_id, \
             reason, occupancy_lease_id, fencing_token, expected_revision, receipt_payload, \
             recorded_at, completed_at FROM worker_stop_intents \
             WHERE idempotency_key = ?1",
            params![idempotency_key],
            row_to_worker_stop_intent,
        )
        .optional()
        .map_err(sql_error)
}

fn row_to_worker_stop_intent(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkerStopIntentRecord> {
    let fencing_token: i64 = row.get(6)?;
    let expected_revision: i64 = row.get(7)?;
    Ok(WorkerStopIntentRecord {
        command_message_id: row.get(0)?,
        idempotency_key: row.get(1)?,
        worker_session_id: row.get(2)?,
        worker_id: row.get(3)?,
        reason: parse_worker_stop_reason_wire_name(&row.get::<_, String>(4)?)?,
        occupancy_lease_id: row.get(5)?,
        fencing_token: u64::try_from(fencing_token).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                "stored worker stop fencing token is negative".into(),
            )
        })?,
        expected_revision: u64::try_from(expected_revision).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Integer,
                "stored worker stop revision is negative".into(),
            )
        })?,
        receipt_payload: row.get(8)?,
        recorded_at: row.get(9)?,
        completed_at: row.get(10)?,
    })
}

fn same_worker_stop_intent(
    stored: &WorkerStopIntentRecord,
    incoming: &WorkerStopIntentRecord,
) -> bool {
    stored.idempotency_key == incoming.idempotency_key
        && stored.worker_session_id == incoming.worker_session_id
        && stored.worker_id == incoming.worker_id
        && stored.reason == incoming.reason
        && stored.occupancy_lease_id == incoming.occupancy_lease_id
        && stored.fencing_token == incoming.fencing_token
        && stored.expected_revision == incoming.expected_revision
}

fn row_to_occupancy_mirror(row: &rusqlite::Row<'_>) -> rusqlite::Result<OccupancyMirrorRecord> {
    let fencing_token: i64 = row.get(1)?;
    let mirror_revision: i64 = row.get(3)?;
    Ok(OccupancyMirrorRecord {
        occupancy_lease_id: row.get(0)?,
        fencing_token: u64::try_from(fencing_token).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Integer,
                "stored occupancy fencing token is negative".into(),
            )
        })?,
        holder_user_id: row.get(2)?,
        mirror_revision: u64::try_from(mirror_revision).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                "stored occupancy mirror revision is negative".into(),
            )
        })?,
        claim_request_id: row.get(4)?,
        idle_expires_at: row.get(5)?,
        acknowledged_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn row_to_release_intent(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<OccupancyReleaseIntentRecord> {
    let fencing_token: i64 = row.get(3)?;
    let affected_worker_sessions: i64 = row.get(5)?;
    Ok(OccupancyReleaseIntentRecord {
        idempotency_key: row.get(0)?,
        command_message_id: row.get(1)?,
        occupancy_lease_id: row.get(2)?,
        fencing_token: u64::try_from(fencing_token).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                "stored release fencing token is negative".into(),
            )
        })?,
        mode: parse_release_mode(&row.get::<_, String>(4)?)?,
        affected_worker_sessions: u64::try_from(affected_worker_sessions).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Integer,
                "stored affected worker count is negative".into(),
            )
        })?,
        recorded_at: row.get(6)?,
    })
}

const fn release_mode_to_str(mode: ClientOccupancyReleaseMode) -> &'static str {
    match mode {
        ClientOccupancyReleaseMode::Immediate => "immediate",
        ClientOccupancyReleaseMode::DrainThenRelease => "drain_then_release",
        ClientOccupancyReleaseMode::CancelTasksAndRelease => "cancel_tasks_and_release",
    }
}

fn parse_release_mode(value: &str) -> rusqlite::Result<ClientOccupancyReleaseMode> {
    match value {
        "immediate" => Ok(ClientOccupancyReleaseMode::Immediate),
        "drain_then_release" => Ok(ClientOccupancyReleaseMode::DrainThenRelease),
        "cancel_tasks_and_release" => Ok(ClientOccupancyReleaseMode::CancelTasksAndRelease),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("stored release mode {other} is not a release mode").into(),
        )),
    }
}

fn validate_mirror_update(update: &OccupancyMirrorUpdate) -> Result<(), DeviceStoreError> {
    require_non_empty(
        &update.occupancy_lease_id,
        "occupancy lease id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&update.acknowledged_at, "acknowledged at", MAX_ID_BYTES)?;
    if update.fencing_token == 0 || update.fencing_token > MAX_SAFE_INTEGER {
        return Err(DeviceStoreError::invalid(
            "occupancy fencing token must be a positive SQLite-range integer",
        ));
    }
    if let Some(holder_user_id) = &update.holder_user_id {
        require_non_empty(holder_user_id, "holder user id", MAX_ID_BYTES)?;
    }
    if let Some(claim_request_id) = &update.claim_request_id {
        require_non_empty(claim_request_id, "claim request id", MAX_ID_BYTES)?;
    }
    Ok(())
}

fn validate_release_intent(intent: &OccupancyReleaseIntentRecord) -> Result<(), DeviceStoreError> {
    require_non_empty(&intent.idempotency_key, "idempotency key", MAX_ID_BYTES)?;
    require_non_empty(
        &intent.command_message_id,
        "command message id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(
        &intent.occupancy_lease_id,
        "occupancy lease id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&intent.recorded_at, "recorded at", MAX_ID_BYTES)?;
    if intent.fencing_token == 0 || intent.fencing_token > MAX_SAFE_INTEGER {
        return Err(DeviceStoreError::invalid(
            "release fencing token must be a positive SQLite-range integer",
        ));
    }
    Ok(())
}

fn validate_worker_stop_intent(intent: &WorkerStopIntentRecord) -> Result<(), DeviceStoreError> {
    require_non_empty(
        &intent.command_message_id,
        "worker stop command message id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&intent.idempotency_key, "idempotency key", MAX_ID_BYTES)?;
    require_non_empty(&intent.worker_session_id, "worker session id", MAX_ID_BYTES)?;
    require_non_empty(&intent.worker_id, "worker id", MAX_ID_BYTES)?;
    require_non_empty(
        &intent.occupancy_lease_id,
        "occupancy lease id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&intent.recorded_at, "recorded at", MAX_ID_BYTES)?;
    if intent.fencing_token == 0 || intent.fencing_token > MAX_SAFE_INTEGER {
        return Err(DeviceStoreError::invalid(
            "worker stop fencing token must be a positive SQLite-range integer",
        ));
    }
    if intent.expected_revision > MAX_SAFE_INTEGER {
        return Err(DeviceStoreError::invalid(
            "worker stop revision must be a SQLite-range integer",
        ));
    }
    if intent
        .receipt_payload
        .as_ref()
        .is_some_and(|payload| payload.is_empty() || payload.len() > 64 * 1024)
    {
        return Err(DeviceStoreError::invalid(
            "worker stop receipt payload must be between 1 and 65536 bytes",
        ));
    }
    if let Some(completed_at) = &intent.completed_at {
        require_non_empty(completed_at, "worker stop completed at", MAX_ID_BYTES)?;
    }
    Ok(())
}

fn validate_worker_process_record(record: &WorkerProcessRecord) -> Result<(), DeviceStoreError> {
    require_non_empty(&record.worker_session_id, "worker session id", MAX_ID_BYTES)?;
    require_non_empty(&record.worker_id, "worker id", MAX_ID_BYTES)?;
    require_non_empty(
        &record.worker_instance_id,
        "worker instance id",
        MAX_ID_BYTES,
    )?;
    if record.pid == 0 {
        return Err(DeviceStoreError::invalid("worker pid must be positive"));
    }
    require_non_empty(
        &record.process_start_identity,
        "process start identity",
        MAX_ID_BYTES,
    )?;
    require_non_empty(
        &record.repository_binding_id,
        "repository binding id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(
        &record.occupancy_lease_id,
        "occupancy lease id",
        MAX_ID_BYTES,
    )?;
    require_non_empty(&record.launch_grant_id, "launch grant id", MAX_ID_BYTES)?;
    require_non_empty(
        &record.data_directory,
        "worker data directory",
        MAX_PATH_BYTES,
    )?;
    require_non_empty(&record.state, "worker state", MAX_ID_BYTES)?;
    require_non_empty(&record.last_observed_at, "observed at", MAX_ID_BYTES)?;
    Ok(())
}

fn row_to_worker_process(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkerProcessRecord> {
    let pid: i64 = row.get(3)?;
    Ok(WorkerProcessRecord {
        worker_session_id: row.get(0)?,
        worker_id: row.get(1)?,
        worker_instance_id: row.get(2)?,
        pid: u32::try_from(pid).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                "stored worker pid is outside the platform pid range".into(),
            )
        })?,
        process_start_identity: row.get(4)?,
        repository_binding_id: row.get(5)?,
        occupancy_lease_id: row.get(6)?,
        launch_grant_id: row.get(7)?,
        data_directory: row.get(8)?,
        state: row.get(9)?,
        exit_code: row.get(10)?,
        last_observed_at: row.get(11)?,
    })
}

const STORE_SCHEMA: &str = r"
CREATE TABLE server_command_results (
    command_message_id TEXT PRIMARY KEY NOT NULL,
    payload BLOB NOT NULL
);
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
-- CLIENT-300.3 (plan 12.2, 12.6, 18.3): the durable occupancy mirror — the
-- device-local execution projection of the Control Plane's occupancy lease.
-- At most one row exists: the absence of a row means the device holds no
-- occupancy and every fenced command fails closed. Only
-- client.occupancy.offer and client.occupancy.force_fence advance the row
-- (a release never mutates it), and every advance bumps mirror_revision, so
-- intents authorized under a previous revision are invalidated. The token
-- bound matches the occupancy ledger's safe-integer bound; the row survives
-- disconnects and restarts (recovery reconciles against it).
CREATE TABLE occupancy_mirror (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    occupancy_lease_id TEXT NOT NULL,
    fencing_token INTEGER NOT NULL
        CHECK (fencing_token > 0 AND fencing_token <= 9007199254740991),
    holder_user_id TEXT,
    mirror_revision INTEGER NOT NULL CHECK (mirror_revision > 0),
    claim_request_id TEXT,
    idle_expires_at TEXT,
    acknowledged_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
-- CLIENT-300.3 (plan 12.4): durable release intents. The worker-stopping
-- execution itself belongs to the worker epic; this lane records the
-- requested mode and the affected worker-session count so a restart can
-- reconcile what the device promised. The release command's idempotency
-- key is the primary key, so a server replay never records twice.
CREATE TABLE occupancy_release_intents (
    idempotency_key TEXT PRIMARY KEY NOT NULL,
    command_message_id TEXT NOT NULL,
    occupancy_lease_id TEXT NOT NULL,
    fencing_token INTEGER NOT NULL
        CHECK (fencing_token > 0 AND fencing_token <= 9007199254740991),
    mode TEXT NOT NULL CHECK (mode IN ('immediate', 'drain_then_release', 'cancel_tasks_and_release')),
    affected_worker_sessions INTEGER NOT NULL CHECK (affected_worker_sessions >= 0),
    recorded_at TEXT NOT NULL
);
-- Plan section 8.2: a PID alone is never sufficient because PIDs are reused;
-- every row additionally binds the process start identity (platform handle).
-- WORKER-100.2: `exit_code` keeps the terminal observation's exit status (a
-- signal death or an unobserved `missing` exit stores NULL).
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
    exit_code INTEGER,
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
-- WORKER-100.3: the stop intent is committed before process-group action;
-- receipt_payload is filled only after the registry reaches a terminal state.
CREATE TABLE worker_stop_intents (
    command_message_id TEXT PRIMARY KEY NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    worker_session_id TEXT NOT NULL,
    worker_id TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN
        ('occupant_requested', 'draining_complete', 'lease_recovered',
         'grant_revoked', 'superseded')),
    occupancy_lease_id TEXT NOT NULL,
    fencing_token INTEGER NOT NULL
        CHECK (fencing_token > 0 AND fencing_token <= 9007199254740991),
    expected_revision INTEGER NOT NULL
        CHECK (expected_revision >= 0 AND expected_revision <= 9007199254740991),
    receipt_payload BLOB,
    recorded_at TEXT NOT NULL,
    completed_at TEXT
);
CREATE INDEX worker_stop_intents_by_session
    ON worker_stop_intents (worker_session_id, recorded_at);
-- LOCAL ONLY (GIT-100.2): the device-local candidate registry. Local
-- candidate git refs and workspace paths never leave the device; only the
-- stable candidate identity rides the client.candidate.retained frame.
-- candidate_id is the stable candidate identity — the frozen candidate
-- commit id per the worker freeze convention — so a repeated retention of
-- the same candidate re-encounters the same primary key and the registry
-- stays idempotent. local_state uses the frozen plan 15 lifecycle
-- vocabulary; `retained_at` is the retention fact stamp the wire receipt
-- reports, `created_at` the stamp of the first recording.
CREATE TABLE candidate_local_refs (
    candidate_id TEXT PRIMARY KEY NOT NULL,
    worker_session_id TEXT NOT NULL,
    repository_binding_id TEXT NOT NULL,
    local_git_ref TEXT NOT NULL,
    local_state TEXT NOT NULL
        CHECK (local_state IN
            ('retained', 'branch_created', 'applied', 'discarded', 'failed')),
    created_at TEXT NOT NULL,
    candidate_ref TEXT NOT NULL,
    candidate_commit TEXT NOT NULL,
    retained_at TEXT NOT NULL
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
-- and publication metadata survive a restart. A refresh replaces the single row at generation + 1.
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
-- into client.hello / client.heartbeat for Server-owned access policy.
CREATE TABLE client_connection_policy (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    accepting_connections INTEGER NOT NULL CHECK (accepting_connections IN (0, 1)),
    lock_state TEXT NOT NULL CHECK (lock_state IN ('unlocked', 'locked')),
    updated_at TEXT NOT NULL
);
";

type WorkerReportSchemaDefinition = (&'static str, &'static str, &'static str);
const WORKER_REPORT_SCHEMA: &[WorkerReportSchemaDefinition] = &[
    ("index", "worker_process_registry_state_lease", "CREATE INDEX worker_process_registry_state_lease ON worker_process_registry(state,occupancy_lease_id)"),
    ("index", "worker_process_registry_active", "CREATE INDEX worker_process_registry_active ON worker_process_registry(occupancy_lease_id,worker_session_id) WHERE state NOT IN ('exited','crashed','missing')"),
    ("table", "worker_report_pending", "CREATE TABLE worker_report_pending (worker_session_id TEXT NOT NULL, family TEXT NOT NULL, PRIMARY KEY(family,worker_session_id))"),
    ("trigger", "worker_report_after_insert", "CREATE TRIGGER worker_report_after_insert AFTER INSERT ON worker_process_registry BEGIN
            INSERT INTO worker_report_pending SELECT NEW.worker_session_id,'reconcile' WHERE NOT EXISTS (SELECT 1 FROM worker_report_pending WHERE family='reconcile' AND worker_session_id=NEW.worker_session_id);
            INSERT INTO worker_report_pending SELECT NEW.worker_session_id,'exit' WHERE NEW.state IN ('exited','crashed','missing') AND NOT EXISTS (SELECT 1 FROM worker_report_pending WHERE family='exit' AND worker_session_id=NEW.worker_session_id); END"),
    ("trigger", "worker_report_after_update", "CREATE TRIGGER worker_report_after_update AFTER UPDATE ON worker_process_registry WHEN OLD.worker_instance_id != NEW.worker_instance_id OR OLD.state != NEW.state OR OLD.exit_code IS NOT NEW.exit_code BEGIN
            INSERT INTO worker_report_pending SELECT NEW.worker_session_id,'reconcile' WHERE NOT EXISTS (SELECT 1 FROM worker_report_pending WHERE family='reconcile' AND worker_session_id=NEW.worker_session_id);
            INSERT INTO worker_report_pending SELECT NEW.worker_session_id,'exit' WHERE NEW.state IN ('exited','crashed','missing') AND NOT EXISTS (SELECT 1 FROM worker_report_pending WHERE family='exit' AND worker_session_id=NEW.worker_session_id); END"),
];
const WORKER_LAUNCH_REPORT_SCHEMA: &[WorkerReportSchemaDefinition] = &[
    ("table", "worker_launch_report_pending", "CREATE TABLE worker_launch_report_pending (launch_grant_id TEXT PRIMARY KEY NOT NULL)"),
    ("trigger", "worker_launch_report_after_insert", "CREATE TRIGGER worker_launch_report_after_insert AFTER INSERT ON worker_launch_receipts BEGIN
            INSERT OR IGNORE INTO worker_launch_report_pending VALUES (NEW.launch_grant_id); END"),
];

fn worker_report_schema_sql(
    transaction: &rusqlite::Transaction<'_>,
    kind: &str,
    name: &str,
) -> Result<Option<String>, DeviceStoreError> {
    transaction
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
            params![kind, name],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)
}

fn normalized_worker_report_sql(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_worker_report_schema(
    transaction: &rusqlite::Transaction<'_>,
    definitions: &[WorkerReportSchemaDefinition],
) -> Result<(), DeviceStoreError> {
    for (kind, name, expected) in definitions {
        if worker_report_schema_sql(transaction, kind, name)?
            .as_deref()
            .map(normalized_worker_report_sql)
            != Some(normalized_worker_report_sql(expected))
        {
            return Err(DeviceStoreError::adapter(format!(
                "worker report schema {name} is not canonical"
            )));
        }
    }
    Ok(())
}

fn initialize_schema(connection: &mut Connection) -> Result<(), DeviceStoreError> {
    // Read the schema identity under the same writer lock as initial creation.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    let version = transaction
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    match version {
        0 => {
            transaction.execute_batch(STORE_SCHEMA).map_err(sql_error)?;
            for (_, _, definition) in WORKER_REPORT_SCHEMA
                .iter()
                .chain(WORKER_LAUNCH_REPORT_SCHEMA)
            {
                transaction.execute_batch(definition).map_err(sql_error)?;
            }
            transaction
                .pragma_update(None, "user_version", CLIENT_STORE_SCHEMA_VERSION)
                .map_err(sql_error)?;
        }
        CLIENT_STORE_SCHEMA_VERSION => {}
        _ => {
            return Err(DeviceStoreError::adapter(format!(
                "unsupported schema version {version}"
            )));
        }
    }
    validate_store_schema(&transaction)?;
    validate_worker_report_schema(&transaction, WORKER_REPORT_SCHEMA)?;
    validate_worker_report_schema(&transaction, WORKER_LAUNCH_REPORT_SCHEMA)?;
    transaction.commit().map_err(sql_error)
}

const STORE_SCHEMA_COLUMNS: &[(&str, &str, &[&str])] = &[
    (
        "server_command_results",
        "PRAGMA table_info(server_command_results)",
        &["command_message_id", "payload"],
    ),
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
            "singleton",
            "occupancy_lease_id",
            "fencing_token",
            "holder_user_id",
            "mirror_revision",
            "claim_request_id",
            "idle_expires_at",
            "acknowledged_at",
            "updated_at",
        ],
    ),
    (
        "occupancy_release_intents",
        "PRAGMA table_info(occupancy_release_intents)",
        &[
            "idempotency_key",
            "command_message_id",
            "occupancy_lease_id",
            "fencing_token",
            "mode",
            "affected_worker_sessions",
            "recorded_at",
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
            "exit_code",
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
        "worker_stop_intents",
        "PRAGMA table_info(worker_stop_intents)",
        &[
            "command_message_id",
            "idempotency_key",
            "worker_session_id",
            "worker_id",
            "reason",
            "occupancy_lease_id",
            "fencing_token",
            "expected_revision",
            "receipt_payload",
            "recorded_at",
            "completed_at",
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
            "candidate_ref",
            "candidate_commit",
            "retained_at",
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

/// A canonical database has one initialization lock across daemon and CLI processes.
/// The OS releases it when the returned file closes, including process termination.
fn acquire_sqlite_process_open_lock(
    database_path: &Path,
    open_deadline: StdInstant,
) -> Result<fs::File, DeviceStoreError> {
    let lock_path = database_path.with_extension("sqlite3.open.lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| {
            DeviceStoreError::adapter(format!("SQLite initialization lock cannot open: {error}"))
        })?;
    loop {
        remaining_sqlite_open_time(open_deadline)?;
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) => {
                #[cfg(test)]
                open_process_concurrency_tests::on_process_open_lock_contention();
                let remaining = remaining_sqlite_open_time(open_deadline)?;
                thread::sleep(remaining.min(SQLITE_OPEN_RETRY_INTERVAL));
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(DeviceStoreError::adapter(format!(
                    "SQLite initialization lock cannot acquire: {error}"
                )));
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

impl DeviceStore {
    pub fn worker_launch_receipt(
        &self,
        grant_id: &str,
    ) -> Result<Option<Vec<u8>>, DeviceStoreError> {
        self.connection()?
            .query_row(
                "SELECT receipt_payload FROM worker_launch_receipts WHERE launch_grant_id=?1",
                [grant_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)
    }
    pub fn worker_launch_receipts(&self) -> Result<Vec<Vec<u8>>, DeviceStoreError> {
        let mut statement = self
            .connection()?
            .prepare("SELECT receipt_payload FROM worker_launch_receipts ORDER BY launch_grant_id")
            .map_err(sql_error)?;
        statement
            .query_map([], |row| row.get(0))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)
    }
    pub fn pending_worker_launch_receipts(
        &self,
    ) -> Result<Vec<(String, Vec<u8>)>, DeviceStoreError> {
        let mut statement = self.connection()?.prepare("SELECT r.launch_grant_id,r.receipt_payload FROM worker_launch_report_pending p JOIN worker_launch_receipts r ON r.launch_grant_id=p.launch_grant_id ORDER BY r.launch_grant_id").map_err(sql_error)?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql_error)
    }

    pub fn worker_launch_report_queued(&mut self, grant: &str) -> Result<(), DeviceStoreError> {
        self.connection_mut()?
            .execute(
                "DELETE FROM worker_launch_report_pending WHERE launch_grant_id=?1",
                [grant],
            )
            .map_err(sql_error)?;
        Ok(())
    }

    pub fn retain_worker_launch_receipt(
        &mut self,
        grant_id: &str,
        session_id: &str,
        status: &str,
        key: &str,
        payload: &[u8],
        now: &str,
    ) -> Result<(), DeviceStoreError> {
        if let Some(existing) = self.worker_launch_receipt(grant_id)? {
            return if existing == payload {
                Ok(())
            } else {
                Err(DeviceStoreError::conflict("Worker launch receipt differs"))
            };
        }
        self.connection()?
            .execute(
                "INSERT INTO worker_launch_receipts VALUES (?1,?2,?3,?4,?5,?6)",
                params![grant_id, session_id, status, key, payload, now],
            )
            .map_err(sql_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod schema_concurrency_tests {
    use super::{
        CLIENT_STORE_SCHEMA_VERSION, STORE_SCHEMA, WORKER_LAUNCH_REPORT_SCHEMA,
        WORKER_REPORT_SCHEMA, initialize_schema,
    };
    use rusqlite::{Connection, TransactionBehavior};
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant as StdInstant};

    static WAITING_FOR_SCHEMA_LOCK: AtomicBool = AtomicBool::new(false);

    #[test]
    fn initialization_reads_version_after_acquiring_the_database_lock() {
        let root =
            std::env::temp_dir().join(format!("wwc-device-schema-race-{}", std::process::id()));
        fs::create_dir_all(&root).expect("create fixture");
        let path = root.join("device-client.sqlite3");
        let mut first = Connection::open(&path).expect("first connection");
        first
            .pragma_update(None, "journal_mode", "WAL")
            .expect("WAL");
        let transaction = first
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("hold schema lock");
        WAITING_FOR_SCHEMA_LOCK.store(false, Ordering::SeqCst);
        let second = std::thread::spawn(move || {
            let mut connection = Connection::open(path).expect("second connection");
            connection
                .busy_handler(Some(|attempt| {
                    WAITING_FOR_SCHEMA_LOCK.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1));
                    attempt < 2_000
                }))
                .expect("observe lock contention");
            initialize_schema(&mut connection)
        });
        let deadline = StdInstant::now() + Duration::from_secs(2);
        while !WAITING_FOR_SCHEMA_LOCK.load(Ordering::SeqCst) && StdInstant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            WAITING_FOR_SCHEMA_LOCK.load(Ordering::SeqCst),
            "second initializer must contend"
        );
        transaction
            .execute_batch(STORE_SCHEMA)
            .expect("first initializer creates schema");
        for (_, _, definition) in WORKER_REPORT_SCHEMA
            .iter()
            .chain(WORKER_LAUNCH_REPORT_SCHEMA)
        {
            transaction
                .execute_batch(definition)
                .expect("report schema");
        }
        transaction
            .pragma_update(None, "user_version", CLIENT_STORE_SCHEMA_VERSION)
            .expect("version");
        transaction.commit().expect("first initializer commits");
        second
            .join()
            .expect("second thread")
            .expect("second initializer must reread committed version");
        drop(first);
        fs::remove_dir_all(root).expect("cleanup");
    }
}

#[cfg(test)]
mod outbox_read_cut_tests {
    use super::{DeviceStore, DeviceStoreError};
    use rusqlite::trace::{TraceEvent, TraceEventCodes};
    use std::cell::RefCell;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;
    use winwincode_client_port::exchange::{
        CompactingOutbox, FrameCodec, FrameOutbox, OutboxSession, OutboxSnapshot, StoredFrame,
    };
    use winwincode_client_port::messages::{
        CLIENT_CONTROL_PORT_SCHEMA_VERSION, ClientRepositoryRemovedPayload, ClientToServerEnvelope,
        ClientToServerMessage, CommandContext,
    };

    type CommitResult = Result<(), String>;

    struct TraceGate {
        start: Sender<()>,
        committed: Receiver<CommitResult>,
    }

    thread_local! {
        static LOAD_GATE: RefCell<Option<TraceGate>> = const { RefCell::new(None) };
        static TRACE_RESULT: RefCell<Option<CommitResult>> = const { RefCell::new(None) };
    }

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    #[derive(Clone, Copy)]
    enum Mutation {
        Append,
        Compact,
    }

    struct Fixture {
        root: PathBuf,
        reader: DeviceStore,
        writer: DeviceStore,
    }

    struct RaceCapture {
        first: Result<Option<OutboxSnapshot>, DeviceStoreError>,
        later: Result<Option<OutboxSnapshot>, DeviceStoreError>,
        trace: Option<CommitResult>,
        writer: CommitResult,
    }

    fn frame(sequence: u64) -> StoredFrame {
        let envelope = ClientToServerEnvelope {
            schema_version: CLIENT_CONTROL_PORT_SCHEMA_VERSION.to_owned(),
            message_id: format!("read-cut-{sequence}"),
            client_node_id: "read-cut-node".to_owned(),
            client_instance_id: "read-cut-instance".to_owned(),
            sequence,
            occurred_at: "2026-10-09T00:00:00.000Z".to_owned(),
            message: ClientToServerMessage::RepositoryRemoved(ClientRepositoryRemovedPayload {
                command: CommandContext {
                    expected_revision: 0,
                    idempotency_key: format!("read-cut-idempotency-{sequence}"),
                },
                repository_binding_id: "read-cut-binding".to_owned(),
            }),
        };
        FrameCodec::default()
            .encode_envelope(&envelope)
            .expect("encode canonical fixture frame")
    }

    fn bind(store: &mut DeviceStore) {
        store
            .bind_outbox_stream("read-cut-node", "read-cut-instance")
            .expect("bind canonical fixture stream");
    }

    fn fixture(mutation: Mutation) -> Fixture {
        let suffix = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wwc-outbox-read-cut-{}-{suffix}",
            std::process::id()
        ));
        let mut reader = DeviceStore::open(&root).expect("open fixture reader");
        bind(&mut reader);
        let session = OutboxSession::new();
        for sequence in 1..=14 {
            session
                .enqueue(&mut reader, sequence, &frame(sequence))
                .expect("retain canonical seed frame");
        }
        if matches!(mutation, Mutation::Append) {
            session
                .acknowledge(&mut reader, 14)
                .expect("acknowledge seed frames");
        }
        let mut writer = DeviceStore::open(&root).expect("pre-open writer before reader cut");
        bind(&mut writer);
        Fixture {
            root,
            reader,
            writer,
        }
    }

    fn trace_pending_frames(event: &TraceEvent<'_>) {
        let TraceEvent::Stmt(_, sql) = event else {
            return;
        };
        if !sql.starts_with("SELECT message_id, envelope_sequence, kind, payload, occurred_at")
            || !sql.contains("published = 0")
        {
            return;
        }
        let Some(gate) = LOAD_GATE.with(|slot| slot.borrow_mut().take()) else {
            return;
        };
        let committed = gate
            .start
            .send(())
            .map_err(|error| error.to_string())
            .and_then(|()| {
                gate.committed
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| error.to_string())?
            });
        TRACE_RESULT.with(|slot| *slot.borrow_mut() = Some(committed));
    }

    fn mutate(writer: &mut DeviceStore, mutation: Mutation) -> CommitResult {
        match mutation {
            Mutation::Append => OutboxSession::new()
                .enqueue(writer, 15, &frame(15))
                .map(|_| ())
                .map_err(|error| format!("atomic append failed: {error:?}")),
            Mutation::Compact => writer
                .compact_through(14)
                .map(|_| ())
                .map_err(|error| format!("atomic compaction failed: {error:?}")),
        }
    }

    fn capture_race(mutation: Mutation) -> RaceCapture {
        let Fixture {
            root,
            mut reader,
            mut writer,
        } = fixture(mutation);
        let (start_tx, start_rx) = mpsc::channel();
        let (committed_tx, committed_rx) = mpsc::channel();
        let join_handle = std::thread::spawn(move || {
            let committed = start_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| error.to_string())
                .and_then(|()| mutate(&mut writer, mutation));
            let _ = committed_tx.send(committed.clone());
            (writer, committed)
        });
        LOAD_GATE.with(|slot| {
            *slot.borrow_mut() = Some(TraceGate {
                start: start_tx,
                committed: committed_rx,
            });
        });
        TRACE_RESULT.with(|slot| *slot.borrow_mut() = None);
        reader.connection().expect("reader connection").trace_v2(
            TraceEventCodes::SQLITE_TRACE_STMT,
            Some(|event| trace_pending_frames(&event)),
        );
        let first = FrameOutbox::load(&mut reader);
        reader
            .connection()
            .expect("reader connection after load")
            .trace_v2(TraceEventCodes::empty(), None);
        LOAD_GATE.with(|slot| drop(slot.borrow_mut().take()));
        let trace = TRACE_RESULT.with(|slot| slot.borrow_mut().take());
        let (writer_store, writer_result) = join_handle.join().expect("join fixture writer");
        let later = FrameOutbox::load(&mut reader);
        writer_store.close().expect("close writer after reader cut");
        reader.close().expect("close reader fixture");
        fs::remove_dir_all(root).expect("remove closed fixture");
        RaceCapture {
            first,
            later,
            trace,
            writer: writer_result,
        }
    }

    fn snapshots(capture: RaceCapture) -> (OutboxSnapshot, OutboxSnapshot) {
        assert_eq!(
            capture.trace,
            Some(Ok(())),
            "pending SELECT must wait until the other connection has committed"
        );
        capture.writer.expect("writer mutation must commit");
        let first = capture.first.expect("load pinned cut").expect("seed rows");
        let later = capture
            .later
            .expect("load fresh cut")
            .expect("durable rows");
        later.validate().expect("fresh load must remain valid");
        (first, later)
    }

    #[test]
    fn frame_outbox_load_pins_cursors_and_frames_during_concurrent_append() {
        let (first, later) = snapshots(capture_race(Mutation::Append));
        first
            .validate()
            .expect("load must retain one SQLite read cut during append");
        assert_eq!(first.ack_sequence, 14);
        assert_eq!(first.highest_sequence, 14);
        assert!(first.frames.is_empty());
        assert_eq!(later.ack_sequence, 14);
        assert_eq!(later.highest_sequence, 15);
        assert_eq!(later.frames, vec![frame(15)]);
    }

    #[test]
    fn frame_outbox_load_pins_cursors_and_frames_during_concurrent_compaction() {
        let (first, later) = snapshots(capture_race(Mutation::Compact));
        first
            .validate()
            .expect("load must retain one SQLite read cut during compaction");
        assert_eq!(first.ack_sequence, 0);
        assert_eq!(first.highest_sequence, 14);
        assert_eq!(first.frames, (1..=14).map(frame).collect::<Vec<_>>());
        assert_eq!(later.ack_sequence, 14);
        assert_eq!(later.highest_sequence, 14);
        assert!(later.frames.is_empty());
    }
}

#[cfg(test)]
#[path = "worker_report_schema_tests.rs"]
mod worker_report_schema_tests;

#[cfg(test)]
#[path = "store_open_process_concurrency_tests.rs"]
mod open_process_concurrency_tests;
