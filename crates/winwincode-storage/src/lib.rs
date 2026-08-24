// SPDX-License-Identifier: Apache-2.0

//! Transactional product-state storage for the `WinWinCode` Control Plane.
//!
//! [`ProductStateStorage`] is the storage seam used by the Control Plane. A
//! commit replaces one canonical state value and appends its outbox events in
//! one transaction. The interface deliberately does not expose transaction
//! handles, so callers cannot publish an event before its state is durable.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{RequestId, Sha256Digest};

const DATABASE_FILE_NAME: &str = "control-plane.sqlite3";
const SCHEMA_VERSION: i64 = 2;
const LEGACY_V1_ACTOR_KEY: &[u8] = b"winwincode.command-receipt.actor.legacy-v1";
const LEGACY_V1_SCOPE_KEY: &[u8] = b"winwincode.command-receipt.scope.legacy-v1";

/// One event to append atomically with a canonical state change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewOutboxEvent {
    pub event_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
}

impl NewOutboxEvent {
    #[must_use]
    pub fn new(
        event_id: impl Into<String>,
        topic: impl Into<String>,
        payload: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            topic: topic.into(),
            payload: payload.into(),
        }
    }
}

/// Opaque canonical actor identity encoded by the Control Plane adapter.
///
/// The storage adapter never receives authentication proof or credentials. It
/// only receives the stable actor identity fields from `CommandEnvelope`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptActorKey(Vec<u8>);

impl ReceiptActorKey {
    /// Builds a typed storage key from the canonical actor fields.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] for an empty encoding.
    pub fn from_encoded(encoded: Vec<u8>) -> Result<Self, StorageError> {
        if encoded.is_empty() {
            return Err(StorageError::invalid("receipt actor key must not be empty"));
        }
        Ok(Self(encoded))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Opaque canonical organization/workspace/project/repository scope encoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptScopeKey(Vec<u8>);

impl ReceiptScopeKey {
    /// Builds a typed storage key from every canonical scope field.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] for an empty encoding.
    pub fn from_encoded(encoded: Vec<u8>) -> Result<Self, StorageError> {
        if encoded.is_empty() {
            return Err(StorageError::invalid("receipt scope key must not be empty"));
        }
        Ok(Self(encoded))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// The complete idempotency identity of one canonical HTTP command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptIdentity {
    actor_key: ReceiptActorKey,
    scope_key: ReceiptScopeKey,
    request_id: RequestId,
}

impl ReceiptIdentity {
    /// Combines actor, full scope, and request id into one receipt identity.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] for an empty request id.
    pub fn new(
        actor_key: ReceiptActorKey,
        scope_key: ReceiptScopeKey,
        request_id: RequestId,
    ) -> Result<Self, StorageError> {
        if request_id.0.is_empty() {
            return Err(StorageError::invalid("request_id must not be empty"));
        }
        Ok(Self {
            actor_key,
            scope_key,
            request_id,
        })
    }

    #[must_use]
    pub const fn actor_key(&self) -> &ReceiptActorKey {
        &self.actor_key
    }

    #[must_use]
    pub const fn scope_key(&self) -> &ReceiptScopeKey {
        &self.scope_key
    }

    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }
}

/// One atomic canonical-state and outbox commit at the storage port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateCommit {
    pub receipt_identity: ReceiptIdentity,
    pub command_digest: Sha256Digest,
    pub stream_id: String,
    pub expected_revision: u64,
    pub state: Vec<u8>,
    pub events: Vec<NewOutboxEvent>,
}

impl StateCommit {
    #[must_use]
    pub fn new(
        receipt_identity: ReceiptIdentity,
        command_digest: Sha256Digest,
        stream_id: impl Into<String>,
        expected_revision: u64,
        state: impl Into<Vec<u8>>,
        events: Vec<NewOutboxEvent>,
    ) -> Self {
        Self {
            receipt_identity,
            command_digest,
            stream_id: stream_id.into(),
            expected_revision,
            state: state.into(),
            events,
        }
    }

    fn validate(&self) -> Result<(), StorageError> {
        validate_sha256_digest(&self.command_digest)?;
        if self.stream_id.is_empty() {
            return Err(StorageError::invalid("stream_id must not be empty"));
        }
        if self.events.is_empty() {
            return Err(StorageError::invalid(
                "a state commit must contain at least one outbox event",
            ));
        }
        if self.expected_revision > i64::MAX as u64 {
            return Err(StorageError::invalid(
                "expected_revision exceeds the SQLite integer range",
            ));
        }

        let mut event_ids = HashSet::with_capacity(self.events.len());
        for event in &self.events {
            if event.event_id.is_empty() {
                return Err(StorageError::invalid("event_id must not be empty"));
            }
            if event.topic.is_empty() {
                return Err(StorageError::invalid("event topic must not be empty"));
            }
            if !event_ids.insert(event.event_id.as_str()) {
                return Err(StorageError::invalid(
                    "event ids must be unique inside one commit",
                ));
            }
        }
        Ok(())
    }
}

/// Canonical state loaded through the storage seam.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredState {
    pub stream_id: String,
    pub revision: u64,
    pub payload: Vec<u8>,
}

/// A durable event waiting to be published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboxEvent {
    pub sequence: u64,
    pub event_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
}

/// The durable result of one state commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    pub receipt_identity: ReceiptIdentity,
    pub stream_id: String,
    pub revision: u64,
    pub event_ids: Vec<String>,
    pub idempotent_replay: bool,
}

/// Stable error categories exposed by storage adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageErrorKind {
    InvalidInput,
    RevisionConflict,
    RequestConflict,
    Adapter,
    Closed,
}

/// Storage failure with an adapter-neutral category.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageError {
    kind: StorageErrorKind,
    message: String,
}

impl StorageError {
    #[must_use]
    pub fn adapter(message: impl Into<String>) -> Self {
        Self {
            kind: StorageErrorKind::Adapter,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            kind: StorageErrorKind::InvalidInput,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::invalid_input(message)
    }

    fn closed() -> Self {
        Self {
            kind: StorageErrorKind::Closed,
            message: "storage is already closed".to_owned(),
        }
    }

    fn revision_conflict(expected: u64, actual: u64) -> Self {
        Self {
            kind: StorageErrorKind::RevisionConflict,
            message: format!("expected revision {expected}, but current revision is {actual}"),
        }
    }

    fn request_conflict(request_id: &RequestId) -> Self {
        Self {
            kind: StorageErrorKind::RequestConflict,
            message: format!(
                "request id {} was already used for another command in this actor and scope",
                request_id.0
            ),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> StorageErrorKind {
        self.kind
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StorageError {}

/// Deep storage seam shared by the `SQLite` adapter and a future `PostgreSQL` adapter.
///
/// `commit` owns the full transaction. `pending_events` and `mark_published`
/// implement an at-least-once outbox with stable event ids.
pub trait ProductStateStorage: Send {
    /// Atomically writes canonical state, its request receipt, and outbox events.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when validation, concurrency control,
    /// request idempotency, or the adapter transaction fails.
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError>;

    /// Loads the current canonical state for one stream.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or storage is closed.
    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError>;

    /// Loads all unpublished events in durable sequence order.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or storage is closed.
    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError>;

    /// Marks one stable event id as published.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the event is missing, the write
    /// fails, or storage is closed.
    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError>;

    /// Deterministically closes the adapter and releases its resources.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when checkpointing or close fails.
    fn close(self: Box<Self>) -> Result<(), StorageError>;
}

/// Local `SQLite` implementation of [`ProductStateStorage`].
pub struct SqliteStorage {
    connection: Option<Connection>,
    database_path: PathBuf,
}

impl SqliteStorage {
    /// Opens the local database and applies all schema migrations before return.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the directory, connection,
    /// durability settings, or schema migration cannot be prepared.
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self, StorageError> {
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(|error| {
            StorageError::adapter(format!("failed to create the data directory: {error}"))
        })?;
        let database_path = data_directory.join(DATABASE_FILE_NAME);
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
        let mut connection =
            Connection::open_with_flags(&database_path, flags).map_err(sql_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error)?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        apply_migrations(&mut connection)?;

        Ok(Self {
            connection: Some(connection),
            database_path,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    fn connection(&self) -> Result<&Connection, StorageError> {
        self.connection.as_ref().ok_or_else(StorageError::closed)
    }

    fn connection_mut(&mut self) -> Result<&mut Connection, StorageError> {
        self.connection.as_mut().ok_or_else(StorageError::closed)
    }
}

impl ProductStateStorage for SqliteStorage {
    fn commit(&mut self, commit: &StateCommit) -> Result<CommitReceipt, StorageError> {
        commit.validate()?;
        let connection = self.connection_mut()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;

        if let Some(prior) = prior_receipt(&transaction, &commit.receipt_identity)? {
            let receipt = replay_receipt(&transaction, commit, prior)?;
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }

        let receipt = append_state_commit(&transaction, commit)?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError> {
        self.connection()?
            .query_row(
                "SELECT revision, payload FROM product_state WHERE stream_id = ?1",
                [stream_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(sql_error)?
            .map(|(revision, payload)| {
                Ok(StoredState {
                    stream_id: stream_id.to_owned(),
                    revision: u64::try_from(revision)
                        .map_err(|_| StorageError::adapter("stored revision is negative"))?,
                    payload,
                })
            })
            .transpose()
    }

    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT sequence, event_id, topic, payload FROM outbox \
                 WHERE published = 0 ORDER BY sequence ASC",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            })
            .map_err(sql_error)?;
        rows.map(|row| {
            let (sequence, event_id, topic, payload) = row.map_err(sql_error)?;
            Ok(OutboxEvent {
                sequence: u64::try_from(sequence)
                    .map_err(|_| StorageError::adapter("outbox sequence is negative"))?,
                event_id,
                topic,
                payload,
            })
        })
        .collect()
    }

    fn mark_published(&mut self, event_id: &str) -> Result<(), StorageError> {
        let changed = self
            .connection_mut()?
            .execute(
                "UPDATE outbox SET published = 1 WHERE event_id = ?1",
                [event_id],
            )
            .map_err(sql_error)?;
        if changed == 0 {
            return Err(StorageError::adapter(format!(
                "outbox event {event_id} does not exist"
            )));
        }
        Ok(())
    }

    fn close(mut self: Box<Self>) -> Result<(), StorageError> {
        let connection = self.connection.take().ok_or_else(StorageError::closed)?;
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(sql_error)?;
        connection.close().map_err(|(_, error)| sql_error(error))?;
        Ok(())
    }
}

struct StoredReceipt {
    command_digest: String,
    stream_id: String,
    revision: i64,
}

fn prior_receipt(
    transaction: &rusqlite::Transaction<'_>,
    identity: &ReceiptIdentity,
) -> Result<Option<StoredReceipt>, StorageError> {
    transaction
        .query_row(
            "SELECT command_digest, stream_id, revision FROM command_receipts \
             WHERE actor_key = ?1 AND scope_key = ?2 AND request_id = ?3",
            params![
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
            ],
            |row| {
                Ok(StoredReceipt {
                    command_digest: row.get(0)?,
                    stream_id: row.get(1)?,
                    revision: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

fn replay_receipt(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    prior: StoredReceipt,
) -> Result<CommitReceipt, StorageError> {
    if prior.command_digest != commit.command_digest.0 {
        return Err(StorageError::request_conflict(
            commit.receipt_identity.request_id(),
        ));
    }
    Ok(CommitReceipt {
        receipt_identity: commit.receipt_identity.clone(),
        stream_id: prior.stream_id,
        revision: u64::try_from(prior.revision)
            .map_err(|_| StorageError::adapter("stored revision is negative"))?,
        event_ids: receipt_event_ids(transaction, &commit.receipt_identity)?,
        idempotent_replay: true,
    })
}

fn append_state_commit(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<CommitReceipt, StorageError> {
    let actual_revision = transaction
        .query_row(
            "SELECT revision FROM product_state WHERE stream_id = ?1",
            [&commit.stream_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sql_error)?
        .unwrap_or(0);
    let actual_revision = u64::try_from(actual_revision)
        .map_err(|_| StorageError::adapter("stored revision is negative"))?;
    if actual_revision != commit.expected_revision {
        return Err(StorageError::revision_conflict(
            commit.expected_revision,
            actual_revision,
        ));
    }

    let expected_revision = i64::try_from(commit.expected_revision)
        .map_err(|_| StorageError::invalid("expected_revision is out of range"))?;
    let revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| StorageError::invalid("revision is out of range"))?;
    append_state(transaction, commit, revision)?;
    append_outbox_events(transaction, commit)?;
    append_receipt(transaction, commit, revision)?;

    Ok(CommitReceipt {
        receipt_identity: commit.receipt_identity.clone(),
        stream_id: commit.stream_id.clone(),
        revision: u64::try_from(revision)
            .map_err(|_| StorageError::adapter("committed revision is negative"))?,
        event_ids: commit
            .events
            .iter()
            .map(|event| event.event_id.clone())
            .collect(),
        idempotent_replay: false,
    })
}

fn append_state(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    revision: i64,
) -> Result<(), StorageError> {
    transaction
        .execute(
            "INSERT INTO product_state (stream_id, revision, payload) VALUES (?1, ?2, ?3) \
             ON CONFLICT(stream_id) DO UPDATE SET revision = excluded.revision, payload = excluded.payload",
            params![commit.stream_id, revision, commit.state],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn append_outbox_events(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
) -> Result<(), StorageError> {
    for event in &commit.events {
        transaction
            .execute(
                "INSERT INTO outbox \
                 (event_id, receipt_actor_key, receipt_scope_key, request_id, topic, payload, published) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    event.event_id,
                    commit.receipt_identity.actor_key().as_bytes(),
                    commit.receipt_identity.scope_key().as_bytes(),
                    commit.receipt_identity.request_id().0,
                    event.topic,
                    event.payload
                ],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn append_receipt(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    revision: i64,
) -> Result<(), StorageError> {
    transaction
        .execute(
            "INSERT INTO command_receipts \
             (actor_key, scope_key, request_id, command_digest, stream_id, revision) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                commit.receipt_identity.actor_key().as_bytes(),
                commit.receipt_identity.scope_key().as_bytes(),
                commit.receipt_identity.request_id().0,
                commit.command_digest.0,
                commit.stream_id,
                revision,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn apply_migrations(connection: &mut Connection) -> Result<(), StorageError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    if !matches!(version, 0 | 1 | SCHEMA_VERSION) {
        return Err(StorageError::adapter(format!(
            "unsupported schema version {version}"
        )));
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    match version {
        0 | SCHEMA_VERSION => create_schema_v2(&transaction)?,
        1 => migrate_v1_to_v2(&transaction)?,
        unsupported => {
            return Err(StorageError::adapter(format!(
                "unsupported schema version {unsupported}"
            )));
        }
    }
    transaction
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)?;

    let migrated_version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    if migrated_version != SCHEMA_VERSION {
        return Err(StorageError::adapter(format!(
            "unsupported schema version {migrated_version}"
        )));
    }
    Ok(())
}

fn create_schema_v2(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS product_state (
                 stream_id TEXT PRIMARY KEY NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS command_receipts (
                 actor_key BLOB NOT NULL,
                 scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 command_digest TEXT NOT NULL,
                 stream_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK (revision > 0),
                 PRIMARY KEY (actor_key, scope_key, request_id)
             );
             CREATE TABLE IF NOT EXISTS outbox (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT UNIQUE NOT NULL,
                 receipt_actor_key BLOB NOT NULL,
                 receipt_scope_key BLOB NOT NULL,
                 request_id TEXT NOT NULL,
                 topic TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 published INTEGER NOT NULL DEFAULT 0 CHECK (published IN (0, 1)),
                 FOREIGN KEY (receipt_actor_key, receipt_scope_key, request_id)
                     REFERENCES command_receipts (actor_key, scope_key, request_id)
                     DEFERRABLE INITIALLY DEFERRED
             );
             CREATE INDEX IF NOT EXISTS outbox_pending_sequence
                 ON outbox (published, sequence);",
        )
        .map_err(sql_error)
}

fn migrate_v1_to_v2(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    transaction
        .execute_batch(
            "ALTER TABLE command_receipts RENAME TO command_receipts_v1;
             ALTER TABLE outbox RENAME TO outbox_v1;",
        )
        .map_err(sql_error)?;
    create_schema_v2(transaction)?;

    let legacy_receipts = {
        let mut statement = transaction
            .prepare(
                "SELECT request_id, command_signature, stream_id, revision \
                 FROM command_receipts_v1 ORDER BY request_id",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?
    };
    for (request_id, signature, stream_id, revision) in legacy_receipts {
        transaction
            .execute(
                "INSERT INTO command_receipts \
                 (actor_key, scope_key, request_id, command_digest, stream_id, revision) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    LEGACY_V1_ACTOR_KEY,
                    LEGACY_V1_SCOPE_KEY,
                    request_id,
                    sha256_digest(&signature).0,
                    stream_id,
                    revision,
                ],
            )
            .map_err(sql_error)?;
    }

    transaction
        .execute(
            "INSERT INTO outbox \
             (sequence, event_id, receipt_actor_key, receipt_scope_key, request_id, topic, payload, published) \
             SELECT sequence, event_id, ?1, ?2, request_id, topic, payload, published \
             FROM outbox_v1 ORDER BY sequence",
            params![LEGACY_V1_ACTOR_KEY, LEGACY_V1_SCOPE_KEY],
        )
        .map_err(sql_error)?;
    transaction
        .execute_batch(
            "DROP TABLE outbox_v1;
             DROP TABLE command_receipts_v1;
             CREATE INDEX IF NOT EXISTS outbox_pending_sequence
                 ON outbox (published, sequence);",
        )
        .map_err(sql_error)?;
    Ok(())
}

fn receipt_event_ids(
    transaction: &rusqlite::Transaction<'_>,
    identity: &ReceiptIdentity,
) -> Result<Vec<String>, StorageError> {
    let mut statement = transaction
        .prepare(
            "SELECT event_id FROM outbox \
             WHERE receipt_actor_key = ?1 AND receipt_scope_key = ?2 AND request_id = ?3 \
             ORDER BY sequence",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(
            params![
                identity.actor_key().as_bytes(),
                identity.scope_key().as_bytes(),
                identity.request_id().0,
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(sql_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
}

fn validate_sha256_digest(digest: &Sha256Digest) -> Result<(), StorageError> {
    let Some(hex) = digest.0.strip_prefix("sha256:") else {
        return Err(StorageError::invalid(
            "command_digest must be a sha256 digest",
        ));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(StorageError::invalid(
            "command_digest must contain 64 lowercase hexadecimal digits",
        ));
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> Sha256Digest {
    let digest = Sha256::digest(bytes);
    Sha256Digest(format!("sha256:{digest:x}"))
}

fn sql_error(error: rusqlite::Error) -> StorageError {
    let message = format!("SQLite operation failed: {error}");
    drop(error);
    StorageError::adapter(message)
}
