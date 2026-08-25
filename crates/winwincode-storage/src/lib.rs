// SPDX-License-Identifier: Apache-2.0

//! Transactional product-state storage for the `WinWinCode` Control Plane.
//!
//! [`ProductStateStorage`] is the storage seam used by the Control Plane. A
//! commit replaces one canonical state value and atomically appends an optional
//! opaque aggregate-journal record, its scoped request receipt, and outbox
//! events. The interface deliberately does not expose transaction handles, so
//! callers cannot publish an event before every durable fact commits.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ControlPlaneEventId, DeliveryId, ProductSessionId, RequestId, Sha256Digest,
};

const DATABASE_FILE_NAME: &str = "control-plane.sqlite3";
const SCHEMA_VERSION: i64 = 4;
const LEGACY_V1_ACTOR_KEY: &[u8] = b"winwincode.command-receipt.actor.legacy-v1";
const LEGACY_V1_SCOPE_KEY: &[u8] = b"winwincode.command-receipt.scope.legacy-v1";

/// One event to append atomically with a canonical state change.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewOutboxEvent {
    pub event_id: String,
    pub topic: String,
    pub payload: Vec<u8>,
    projection_stream: Option<ProjectionEventStream>,
}

/// Closed resource stream used to hand an HTTP snapshot to WebSocket replay.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ProjectionEventStream {
    Delivery(DeliveryId),
    ProductSession(ProductSessionId),
}

impl ProjectionEventStream {
    fn kind(&self) -> &'static str {
        match self {
            Self::Delivery(_) => "delivery",
            Self::ProductSession(_) => "product-session",
        }
    }

    fn resource_id(&self) -> &str {
        match self {
            Self::Delivery(delivery_id) => &delivery_id.0,
            Self::ProductSession(product_session_id) => &product_session_id.0,
        }
    }

    fn validate(&self) -> Result<(), StorageError> {
        let (prefix, value) = match self {
            Self::Delivery(delivery_id) => ("delivery", delivery_id.0.as_str()),
            Self::ProductSession(product_session_id) => {
                ("product-session", product_session_id.0.as_str())
            }
        };
        if value.is_empty() || value.len() > 200 || !portable_event_key(value) {
            return Err(StorageError::invalid(format!(
                "{prefix} event stream identity is invalid"
            )));
        }
        Ok(())
    }

    fn from_stored(kind: &str, resource_id: String) -> Result<Self, StorageError> {
        let stream = match kind {
            "delivery" => Self::Delivery(DeliveryId(resource_id)),
            "product-session" => Self::ProductSession(ProductSessionId(resource_id)),
            _ => return Err(StorageError::adapter("stored event stream kind is invalid")),
        };
        stream.validate()?;
        Ok(stream)
    }
}

/// Exact tenant scope and resource stream key owned by durable storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionEventStreamKey {
    scope_key: ReceiptScopeKey,
    stream: ProjectionEventStream,
}

impl ProjectionEventStreamKey {
    /// Creates one exact event stream key.
    ///
    /// # Errors
    ///
    /// Rejects an invalid resource identity.
    pub fn new(
        scope_key: ReceiptScopeKey,
        stream: ProjectionEventStream,
    ) -> Result<Self, StorageError> {
        stream.validate()?;
        Ok(Self { scope_key, stream })
    }

    #[must_use]
    pub const fn scope_key(&self) -> &ReceiptScopeKey {
        &self.scope_key
    }

    #[must_use]
    pub const fn stream(&self) -> &ProjectionEventStream {
        &self.stream
    }
}

/// Last retained event included in one complete HTTP snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionEventCursor {
    key: ProjectionEventStreamKey,
    sequence: u64,
    event_id: Option<ControlPlaneEventId>,
}

impl ProjectionEventCursor {
    /// Rebuilds an exact cursor supplied by the typed application service.
    ///
    /// # Errors
    ///
    /// Rejects a sequence/event identity mismatch or unsafe public value.
    pub fn try_new(
        key: ProjectionEventStreamKey,
        sequence: u64,
        event_id: Option<ControlPlaneEventId>,
    ) -> Result<Self, StorageError> {
        if sequence > 9_007_199_254_740_991
            || (sequence == 0) != event_id.is_none()
            || event_id
                .as_ref()
                .is_some_and(|event_id| !canonical_control_plane_event_id(&event_id.0))
        {
            return Err(StorageError::adapter(
                "stored projection event cursor is invalid",
            ));
        }
        Ok(Self {
            key,
            sequence,
            event_id,
        })
    }

    #[must_use]
    pub const fn key(&self) -> &ProjectionEventStreamKey {
        &self.key
    }

    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    pub const fn event_id(&self) -> Option<&ControlPlaneEventId> {
        self.event_id.as_ref()
    }
}

/// Stable opaque identity for one append-only domain journal.
///
/// Storage treats both fields as bound data. The Control Plane adapter owns
/// their domain meaning, so this crate does not depend on Delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateJournalKey {
    aggregate_type: String,
    aggregate_id: String,
}

impl AggregateJournalKey {
    /// Builds one opaque aggregate journal identity.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::InvalidInput`] when either component is empty.
    pub fn new(
        aggregate_type: impl Into<String>,
        aggregate_id: impl Into<String>,
    ) -> Result<Self, StorageError> {
        let aggregate_type = aggregate_type.into();
        let aggregate_id = aggregate_id.into();
        if aggregate_type.is_empty() || aggregate_id.is_empty() {
            return Err(StorageError::invalid(
                "aggregate journal type and id must not be empty",
            ));
        }
        Ok(Self {
            aggregate_type,
            aggregate_id,
        })
    }

    #[must_use]
    pub fn aggregate_type(&self) -> &str {
        &self.aggregate_type
    }

    #[must_use]
    pub fn aggregate_id(&self) -> &str {
        &self.aggregate_id
    }
}

/// One opaque, digest-addressed append-only journal record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AggregateJournalRecord {
    pub sequence: u64,
    pub digest: String,
    pub payload: Vec<u8>,
}

impl AggregateJournalRecord {
    #[must_use]
    pub fn new(sequence: u64, digest: impl Into<String>, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            sequence,
            digest: digest.into(),
            payload: payload.into(),
        }
    }
}

/// Fully committed opaque journal bytes loaded through the storage port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedAggregateJournal {
    pub manifest: Vec<u8>,
    pub records: Vec<AggregateJournalRecord>,
}

/// One journal create or tail-CAS append staged in a state transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AggregateJournalPublication {
    Create {
        key: AggregateJournalKey,
        manifest: Vec<u8>,
        first_record: AggregateJournalRecord,
    },
    Append {
        key: AggregateJournalKey,
        expected_tail_sequence: u64,
        expected_tail_digest: String,
        record: AggregateJournalRecord,
    },
}

impl AggregateJournalPublication {
    fn validate(&self) -> Result<(), StorageError> {
        match self {
            Self::Create {
                manifest,
                first_record,
                ..
            } => {
                if manifest.is_empty() {
                    return Err(StorageError::invalid(
                        "aggregate journal manifest must not be empty",
                    ));
                }
                validate_journal_record(first_record)?;
                if first_record.sequence != 1 {
                    return Err(StorageError::invalid(
                        "aggregate journal first record sequence must be 1",
                    ));
                }
            }
            Self::Append {
                expected_tail_sequence,
                expected_tail_digest,
                record,
                ..
            } => {
                validate_journal_record(record)?;
                if *expected_tail_sequence == 0 || expected_tail_digest.is_empty() {
                    return Err(StorageError::invalid(
                        "aggregate journal expected tail must be complete",
                    ));
                }
                if record.sequence != expected_tail_sequence.saturating_add(1) {
                    return Err(StorageError::invalid(
                        "aggregate journal append sequence must follow the expected tail",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn validate_journal_record(record: &AggregateJournalRecord) -> Result<(), StorageError> {
    if record.sequence == 0 || record.sequence > i64::MAX as u64 {
        return Err(StorageError::invalid(
            "aggregate journal sequence is outside the SQLite range",
        ));
    }
    if record.digest.is_empty() || record.payload.is_empty() {
        return Err(StorageError::invalid(
            "aggregate journal digest and payload must not be empty",
        ));
    }
    Ok(())
}

impl NewOutboxEvent {
    /// Creates an internal event that is never used as a public replay cursor.
    #[must_use]
    pub fn internal(
        event_id: impl Into<String>,
        topic: impl Into<String>,
        payload: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            event_id: event_id.into(),
            topic: topic.into(),
            payload: payload.into(),
            projection_stream: None,
        }
    }

    /// Creates a secret-safe event in one resource-local projection stream.
    #[must_use]
    pub fn projection(
        event_id: ControlPlaneEventId,
        topic: impl Into<String>,
        payload: impl Into<Vec<u8>>,
        stream: ProjectionEventStream,
    ) -> Self {
        Self {
            event_id: event_id.0,
            topic: topic.into(),
            payload: payload.into(),
            projection_stream: Some(stream),
        }
    }

    #[must_use]
    pub const fn projection_stream(&self) -> Option<&ProjectionEventStream> {
        self.projection_stream.as_ref()
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
    journal_publication: Option<AggregateJournalPublication>,
    receipt_replay_required: bool,
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
            journal_publication: None,
            receipt_replay_required: false,
        }
    }

    /// Adds one opaque aggregate publication to the same state transaction.
    #[must_use]
    pub fn with_journal_publication(mut self, publication: AggregateJournalPublication) -> Self {
        self.journal_publication = Some(publication);
        self
    }

    #[must_use]
    pub const fn journal_publication(&self) -> Option<&AggregateJournalPublication> {
        self.journal_publication.as_ref()
    }

    /// Requires this call to resolve an already durable scoped receipt.
    ///
    /// Aggregate adapters use this after their journal reports the request as
    /// a replay. If an older unsafe composition left only the journal record,
    /// storage fails closed instead of persisting recomputed state or events.
    #[must_use]
    pub fn require_receipt_replay(mut self) -> Self {
        self.receipt_replay_required = true;
        self
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
        if let Some(publication) = &self.journal_publication {
            publication.validate()?;
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
            if let Some(stream) = event.projection_stream() {
                stream.validate()?;
                if !canonical_control_plane_event_id(&event.event_id) {
                    return Err(StorageError::invalid(
                        "projection event_id must be a canonical ControlPlaneEventId",
                    ));
                }
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
    /// Present only for a secret-safe resource-stream event.
    pub projection_cursor: Option<ProjectionEventCursor>,
}

/// The durable result of one state commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitReceipt {
    pub receipt_identity: ReceiptIdentity,
    pub stream_id: String,
    pub revision: u64,
    /// Exact durable events attached to the original scoped request.
    ///
    /// Replays return these stored bytes rather than values recomputed by a
    /// retry, so application adapters can recover the original dispatch job.
    pub events: Vec<OutboxEvent>,
    pub idempotent_replay: bool,
}

/// Stable error categories exposed by storage adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageErrorKind {
    InvalidInput,
    RevisionConflict,
    RequestConflict,
    RequestReplayMissing,
    JournalAlreadyExists,
    JournalNotFound,
    JournalConflict,
    EventCursorExpired,
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

    /// Builds the adapter-neutral concurrency result used by application
    /// adapters that already validated a typed aggregate revision conflict.
    #[must_use]
    pub fn revision_conflict(expected: u64, actual: u64) -> Self {
        Self {
            kind: StorageErrorKind::RevisionConflict,
            message: format!("expected revision {expected}, but current revision is {actual}"),
        }
    }

    /// Builds the revision-token conflict for the one reviewed solution-set
    /// digest accepted at the task-promotion boundary.
    #[must_use]
    pub fn revision_token_conflict(field: &str) -> Self {
        if field != "reviewSetSha256" {
            return Self::invalid_input("revision token conflict field is unsupported");
        }
        Self {
            kind: StorageErrorKind::RevisionConflict,
            message: "reviewSetSha256 no longer identifies the current solution review".to_owned(),
        }
    }

    /// Builds the adapter-neutral result for a reused scoped request whose
    /// canonical command digest differs from the first committed command.
    #[must_use]
    pub fn request_conflict(request_id: &RequestId) -> Self {
        Self {
            kind: StorageErrorKind::RequestConflict,
            message: format!(
                "request id {} was already used for another command in this actor and scope",
                request_id.0
            ),
        }
    }

    fn request_replay_missing(request_id: &RequestId) -> Self {
        Self {
            kind: StorageErrorKind::RequestReplayMissing,
            message: format!(
                "request id {} exists in the aggregate journal without its scoped command receipt",
                request_id.0
            ),
        }
    }

    fn journal_already_exists(key: &AggregateJournalKey) -> Self {
        Self {
            kind: StorageErrorKind::JournalAlreadyExists,
            message: format!(
                "{} aggregate journal {} already exists",
                key.aggregate_type, key.aggregate_id
            ),
        }
    }

    fn journal_not_found(key: &AggregateJournalKey) -> Self {
        Self {
            kind: StorageErrorKind::JournalNotFound,
            message: format!(
                "{} aggregate journal {} does not exist",
                key.aggregate_type, key.aggregate_id
            ),
        }
    }

    fn journal_conflict(key: &AggregateJournalKey) -> Self {
        Self {
            kind: StorageErrorKind::JournalConflict,
            message: format!(
                "{} aggregate journal {} tail changed",
                key.aggregate_type, key.aggregate_id
            ),
        }
    }

    fn event_cursor_expired() -> Self {
        Self {
            kind: StorageErrorKind::EventCursorExpired,
            message: "projection event cursor is outside the retained stream window".to_owned(),
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

    /// Loads the original durable result for one scoped command identity.
    ///
    /// A matching receipt is returned as an idempotent replay. Reusing the
    /// same actor, scope, and request id with another command digest fails
    /// with [`StorageErrorKind::RequestConflict`].
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the digest is malformed, the
    /// scoped request conflicts, or the read fails.
    fn load_receipt(
        &self,
        identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError>;

    /// Loads the current canonical state for one stream.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or storage is closed.
    fn load_state(&self, stream_id: &str) -> Result<Option<StoredState>, StorageError>;

    /// Loads one fully committed opaque aggregate journal.
    ///
    /// # Errors
    ///
    /// Returns an adapter-neutral error when the read fails or storage is closed.
    fn load_journal(
        &self,
        key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError>;

    /// Loads the latest retained position, or validates one exact historical
    /// position, in a tenant-scoped resource event stream.
    ///
    /// # Errors
    ///
    /// Returns [`StorageErrorKind::EventCursorExpired`] only when a previously
    /// persisted positive position is no longer retained.
    fn load_projection_event_cursor(
        &self,
        _key: &ProjectionEventStreamKey,
        _expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionEventCursor, StorageError> {
        Err(StorageError::adapter(
            "projection event stream storage is unavailable",
        ))
    }

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
        if commit.receipt_replay_required {
            return Err(StorageError::request_replay_missing(
                commit.receipt_identity.request_id(),
            ));
        }

        let receipt = append_state_commit(&transaction, commit)?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    fn load_receipt(
        &self,
        identity: &ReceiptIdentity,
        command_digest: &Sha256Digest,
    ) -> Result<Option<CommitReceipt>, StorageError> {
        validate_sha256_digest(command_digest)?;
        let connection = self.connection()?;
        let Some(prior) = prior_receipt(connection, identity)? else {
            return Ok(None);
        };
        replay_stored_receipt(connection, identity, command_digest, prior).map(Some)
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

    fn load_journal(
        &self,
        key: &AggregateJournalKey,
    ) -> Result<Option<LoadedAggregateJournal>, StorageError> {
        load_aggregate_journal(self.connection()?, key)
    }

    fn load_projection_event_cursor(
        &self,
        key: &ProjectionEventStreamKey,
        expected: Option<&ProjectionEventCursor>,
    ) -> Result<ProjectionEventCursor, StorageError> {
        key.stream().validate()?;
        if let Some(expected) = expected {
            if expected.key() != key {
                return Err(StorageError::invalid(
                    "projection event cursor belongs to another scope or stream",
                ));
            }
            if expected.sequence() == 0 {
                return ProjectionEventCursor::try_new(key.clone(), 0, None);
            }
            let connection = self.connection()?;
            let stored_event_id = connection
                .query_row(
                    "SELECT event_id FROM outbox \
                     WHERE receipt_scope_key = ?1 AND projection_stream_kind = ?2 \
                       AND projection_resource_id = ?3 AND projection_stream_sequence = ?4",
                    params![
                        key.scope_key().as_bytes(),
                        key.stream().kind(),
                        key.stream().resource_id(),
                        i64::try_from(expected.sequence()).map_err(|_| {
                            StorageError::invalid("projection event sequence is out of range")
                        })?,
                    ],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(sql_error)?;
            let Some(stored_event_id) = stored_event_id else {
                let Some((head_sequence, _)) = load_projection_stream_head(connection, key)? else {
                    return Err(StorageError::invalid(
                        "projection event cursor was never issued for this stream",
                    ));
                };
                if expected.sequence() > head_sequence {
                    return Err(StorageError::invalid(
                        "projection event cursor is beyond the durable stream head",
                    ));
                }
                return Err(StorageError::event_cursor_expired());
            };
            if expected.event_id().map(|event_id| event_id.0.as_str())
                != Some(stored_event_id.as_str())
            {
                return Err(StorageError::invalid(
                    "projection event cursor eventId does not match its stored position",
                ));
            }
            return ProjectionEventCursor::try_new(
                key.clone(),
                expected.sequence(),
                expected.event_id().cloned(),
            );
        }

        match load_projection_stream_head(self.connection()?, key)? {
            Some((sequence, event_id)) => ProjectionEventCursor::try_new(
                key.clone(),
                sequence,
                Some(ControlPlaneEventId(event_id)),
            ),
            None => ProjectionEventCursor::try_new(key.clone(), 0, None),
        }
    }

    fn pending_events(&self) -> Result<Vec<OutboxEvent>, StorageError> {
        let mut statement = self
            .connection()?
            .prepare(
                "SELECT sequence, event_id, topic, payload, receipt_scope_key, \
                        projection_stream_kind, projection_resource_id, \
                        projection_stream_sequence FROM outbox \
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
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ))
            })
            .map_err(sql_error)?;
        rows.map(|row| {
            let (
                sequence,
                event_id,
                topic,
                payload,
                scope_key,
                stream_kind,
                resource_id,
                stream_sequence,
            ) = row.map_err(sql_error)?;
            Ok(OutboxEvent {
                sequence: u64::try_from(sequence)
                    .map_err(|_| StorageError::adapter("outbox sequence is negative"))?,
                projection_cursor: stored_projection_cursor(
                    scope_key,
                    stream_kind,
                    resource_id,
                    stream_sequence,
                    &event_id,
                )?,
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
    connection: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<StoredReceipt>, StorageError> {
    connection
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
    replay_stored_receipt(
        transaction,
        &commit.receipt_identity,
        &commit.command_digest,
        prior,
    )
}

fn replay_stored_receipt(
    connection: &Connection,
    identity: &ReceiptIdentity,
    command_digest: &Sha256Digest,
    prior: StoredReceipt,
) -> Result<CommitReceipt, StorageError> {
    if prior.command_digest != command_digest.0 {
        return Err(StorageError::request_conflict(identity.request_id()));
    }
    Ok(CommitReceipt {
        receipt_identity: identity.clone(),
        stream_id: prior.stream_id,
        revision: u64::try_from(prior.revision)
            .map_err(|_| StorageError::adapter("stored revision is negative"))?,
        events: receipt_events(connection, identity)?,
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
    if let Some(publication) = commit.journal_publication() {
        append_journal_publication(transaction, publication)?;
    }
    append_receipt(transaction, commit, revision)?;
    append_outbox_events(transaction, commit)?;

    Ok(CommitReceipt {
        receipt_identity: commit.receipt_identity.clone(),
        stream_id: commit.stream_id.clone(),
        revision: u64::try_from(revision)
            .map_err(|_| StorageError::adapter("committed revision is negative"))?,
        events: receipt_events(transaction, &commit.receipt_identity)?,
        idempotent_replay: false,
    })
}

fn append_journal_publication(
    transaction: &rusqlite::Transaction<'_>,
    publication: &AggregateJournalPublication,
) -> Result<(), StorageError> {
    match publication {
        AggregateJournalPublication::Create {
            key,
            manifest,
            first_record,
        } => {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM aggregate_journals \
                     WHERE aggregate_type = ?1 AND aggregate_id = ?2",
                    params![key.aggregate_type(), key.aggregate_id()],
                    |_| Ok(()),
                )
                .optional()
                .map_err(sql_error)?
                .is_some();
            if exists {
                return Err(StorageError::journal_already_exists(key));
            }
            transaction
                .execute(
                    "INSERT INTO aggregate_journals \
                     (aggregate_type, aggregate_id, manifest) VALUES (?1, ?2, ?3)",
                    params![key.aggregate_type(), key.aggregate_id(), manifest],
                )
                .map_err(sql_error)?;
            insert_journal_record(transaction, key, first_record)?;
        }
        AggregateJournalPublication::Append {
            key,
            expected_tail_sequence,
            expected_tail_digest,
            record,
        } => {
            let tail = transaction
                .query_row(
                    "SELECT sequence, digest FROM aggregate_journal_records \
                     WHERE aggregate_type = ?1 AND aggregate_id = ?2 \
                     ORDER BY sequence DESC LIMIT 1",
                    params![key.aggregate_type(), key.aggregate_id()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(sql_error)?;
            let Some((tail_sequence, tail_digest)) = tail else {
                return Err(StorageError::journal_not_found(key));
            };
            let expected_sequence = i64::try_from(*expected_tail_sequence)
                .map_err(|_| StorageError::invalid("expected journal tail is out of range"))?;
            if tail_sequence != expected_sequence || tail_digest != *expected_tail_digest {
                return Err(StorageError::journal_conflict(key));
            }
            insert_journal_record(transaction, key, record)?;
        }
    }
    Ok(())
}

fn insert_journal_record(
    transaction: &rusqlite::Transaction<'_>,
    key: &AggregateJournalKey,
    record: &AggregateJournalRecord,
) -> Result<(), StorageError> {
    let sequence = i64::try_from(record.sequence)
        .map_err(|_| StorageError::invalid("journal sequence is out of range"))?;
    transaction
        .execute(
            "INSERT INTO aggregate_journal_records \
             (aggregate_type, aggregate_id, sequence, digest, payload) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key.aggregate_type(),
                key.aggregate_id(),
                sequence,
                record.digest,
                record.payload,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
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
        let stream_position = event
            .projection_stream()
            .map(|stream| next_projection_stream_position(transaction, commit, stream))
            .transpose()?;
        transaction
            .execute(
                "INSERT INTO outbox \
                 (event_id, receipt_actor_key, receipt_scope_key, request_id, topic, payload, \
                  published, projection_stream_kind, projection_resource_id, \
                  projection_stream_sequence) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9)",
                params![
                    event.event_id,
                    commit.receipt_identity.actor_key().as_bytes(),
                    commit.receipt_identity.scope_key().as_bytes(),
                    commit.receipt_identity.request_id().0,
                    event.topic,
                    event.payload,
                    event.projection_stream().map(ProjectionEventStream::kind),
                    event
                        .projection_stream()
                        .map(ProjectionEventStream::resource_id),
                    stream_position,
                ],
            )
            .map_err(sql_error)?;
        if let (Some(stream), Some(position)) = (event.projection_stream(), stream_position) {
            transaction
                .execute(
                    "INSERT INTO projection_event_stream_heads \
                     (scope_key, stream_kind, resource_id, sequence, event_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT(scope_key, stream_kind, resource_id) DO UPDATE SET \
                         sequence = excluded.sequence, event_id = excluded.event_id",
                    params![
                        commit.receipt_identity.scope_key().as_bytes(),
                        stream.kind(),
                        stream.resource_id(),
                        position,
                        event.event_id,
                    ],
                )
                .map_err(sql_error)?;
        }
    }
    Ok(())
}

fn next_projection_stream_position(
    transaction: &rusqlite::Transaction<'_>,
    commit: &StateCommit,
    stream: &ProjectionEventStream,
) -> Result<i64, StorageError> {
    let current = transaction
        .query_row(
            "SELECT sequence FROM projection_event_stream_heads \
             WHERE scope_key = ?1 AND stream_kind = ?2 AND resource_id = ?3",
            params![
                commit.receipt_identity.scope_key().as_bytes(),
                stream.kind(),
                stream.resource_id(),
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sql_error)?
        .unwrap_or(0);
    if !(0..9_007_199_254_740_991).contains(&current) {
        return Err(StorageError::adapter(
            "projection event stream sequence is outside the public range",
        ));
    }
    current
        .checked_add(1)
        .ok_or_else(|| StorageError::adapter("projection event stream sequence overflow"))
}

fn load_projection_stream_head(
    connection: &Connection,
    key: &ProjectionEventStreamKey,
) -> Result<Option<(u64, String)>, StorageError> {
    connection
        .query_row(
            "SELECT sequence, event_id FROM projection_event_stream_heads \
             WHERE scope_key = ?1 AND stream_kind = ?2 AND resource_id = ?3",
            params![
                key.scope_key().as_bytes(),
                key.stream().kind(),
                key.stream().resource_id(),
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?
        .map(|(sequence, event_id)| {
            u64::try_from(sequence)
                .map(|sequence| (sequence, event_id))
                .map_err(|_| StorageError::adapter("event sequence is negative"))
        })
        .transpose()
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
    if !matches!(version, 0 | 1 | 2 | 3 | SCHEMA_VERSION) {
        return Err(StorageError::adapter(format!(
            "unsupported schema version {version}"
        )));
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    match version {
        0 | SCHEMA_VERSION => create_schema_v4(&transaction)?,
        1 => {
            migrate_v1_to_v2(&transaction)?;
            create_aggregate_journal_schema(&transaction)?;
            create_projection_event_schema(&transaction)?;
        }
        2 => {
            create_aggregate_journal_schema(&transaction)?;
            create_projection_event_schema(&transaction)?;
        }
        3 => create_projection_event_schema(&transaction)?,
        unsupported => {
            return Err(StorageError::adapter(format!(
                "unsupported schema version {unsupported}"
            )));
        }
    }
    validate_journal_schema(&transaction)?;
    validate_projection_event_schema(&transaction)?;
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

fn validate_journal_schema(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    let journal_columns = {
        let mut statement = transaction
            .prepare("PRAGMA table_info(aggregate_journals)")
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?
    };
    if journal_columns != ["aggregate_type", "aggregate_id", "manifest"] {
        return Err(StorageError::adapter(
            "aggregate journal schema is not canonical",
        ));
    }
    let record_columns = {
        let mut statement = transaction
            .prepare("PRAGMA table_info(aggregate_journal_records)")
            .map_err(sql_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)?
    };
    if record_columns
        != [
            "aggregate_type",
            "aggregate_id",
            "sequence",
            "digest",
            "payload",
        ]
    {
        return Err(StorageError::adapter(
            "aggregate journal record schema is not canonical",
        ));
    }
    Ok(())
}

fn create_schema_v3(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    create_schema_v2(transaction)?;
    create_aggregate_journal_schema(transaction)
}

fn create_schema_v4(transaction: &rusqlite::Transaction<'_>) -> Result<(), StorageError> {
    create_schema_v3(transaction)?;
    create_projection_event_schema(transaction)
}

fn create_projection_event_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    let columns = table_columns(transaction, "outbox")?;
    if !columns.contains(&"projection_stream_kind".to_owned()) {
        transaction
            .execute_batch(
                "ALTER TABLE outbox ADD COLUMN projection_stream_kind TEXT;
                 ALTER TABLE outbox ADD COLUMN projection_resource_id TEXT;
                 ALTER TABLE outbox ADD COLUMN projection_stream_sequence INTEGER;",
            )
            .map_err(sql_error)?;
    }
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS projection_event_stream_heads (
                 scope_key BLOB NOT NULL,
                 stream_kind TEXT NOT NULL CHECK (stream_kind IN ('delivery', 'product-session')),
                 resource_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 event_id TEXT NOT NULL,
                 PRIMARY KEY (scope_key, stream_kind, resource_id),
                 UNIQUE (scope_key, stream_kind, resource_id, sequence),
                 FOREIGN KEY (event_id) REFERENCES outbox (event_id)
             );
             CREATE UNIQUE INDEX IF NOT EXISTS outbox_projection_stream_sequence
                 ON outbox (receipt_scope_key, projection_stream_kind,
                            projection_resource_id, projection_stream_sequence)
                 WHERE projection_stream_kind IS NOT NULL;",
        )
        .map_err(sql_error)
}

fn validate_projection_event_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    let columns = table_columns(transaction, "outbox")?;
    let required = [
        "projection_stream_kind",
        "projection_resource_id",
        "projection_stream_sequence",
    ];
    if !required
        .iter()
        .all(|column| columns.iter().any(|value| value == column))
    {
        return Err(StorageError::adapter(
            "projection event outbox schema is not canonical",
        ));
    }
    let head_columns = table_columns(transaction, "projection_event_stream_heads")?;
    if head_columns
        != [
            "scope_key",
            "stream_kind",
            "resource_id",
            "sequence",
            "event_id",
        ]
    {
        return Err(StorageError::adapter(
            "projection event stream head schema is not canonical",
        ));
    }
    Ok(())
}

fn table_columns(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
) -> Result<Vec<String>, StorageError> {
    let pragma = match table {
        "outbox" => "PRAGMA table_info(outbox)",
        "projection_event_stream_heads" => "PRAGMA table_info(projection_event_stream_heads)",
        _ => return Err(StorageError::adapter("unknown schema table")),
    };
    let mut statement = transaction.prepare(pragma).map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sql_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
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

fn create_aggregate_journal_schema(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<(), StorageError> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS aggregate_journals (
                 aggregate_type TEXT NOT NULL,
                 aggregate_id TEXT NOT NULL,
                 manifest BLOB NOT NULL,
                 PRIMARY KEY (aggregate_type, aggregate_id)
             );
             CREATE TABLE IF NOT EXISTS aggregate_journal_records (
                 aggregate_type TEXT NOT NULL,
                 aggregate_id TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 digest TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 PRIMARY KEY (aggregate_type, aggregate_id, sequence),
                 FOREIGN KEY (aggregate_type, aggregate_id)
                     REFERENCES aggregate_journals (aggregate_type, aggregate_id)
                     ON DELETE CASCADE
             );",
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

fn receipt_events(
    connection: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Vec<OutboxEvent>, StorageError> {
    let mut statement = connection
        .prepare(
            "SELECT sequence, event_id, topic, payload, receipt_scope_key, \
                    projection_stream_kind, projection_resource_id, \
                    projection_stream_sequence FROM outbox \
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
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ))
            },
        )
        .map_err(sql_error)?;
    rows.map(|row| {
        let (
            sequence,
            event_id,
            topic,
            payload,
            scope_key,
            stream_kind,
            resource_id,
            stream_sequence,
        ) = row.map_err(sql_error)?;
        Ok(OutboxEvent {
            sequence: u64::try_from(sequence)
                .map_err(|_| StorageError::adapter("outbox sequence is negative"))?,
            projection_cursor: stored_projection_cursor(
                scope_key,
                stream_kind,
                resource_id,
                stream_sequence,
                &event_id,
            )?,
            event_id,
            topic,
            payload,
        })
    })
    .collect()
}

fn stored_projection_cursor(
    scope_key: Vec<u8>,
    stream_kind: Option<String>,
    resource_id: Option<String>,
    stream_sequence: Option<i64>,
    event_id: &str,
) -> Result<Option<ProjectionEventCursor>, StorageError> {
    match (stream_kind, resource_id, stream_sequence) {
        (None, None, None) => Ok(None),
        (Some(stream_kind), Some(resource_id), Some(stream_sequence)) => {
            let stream = ProjectionEventStream::from_stored(&stream_kind, resource_id)?;
            let key =
                ProjectionEventStreamKey::new(ReceiptScopeKey::from_encoded(scope_key)?, stream)?;
            ProjectionEventCursor::try_new(
                key,
                u64::try_from(stream_sequence).map_err(|_| {
                    StorageError::adapter("stored projection event sequence is negative")
                })?,
                Some(ControlPlaneEventId(event_id.to_owned())),
            )
            .map(Some)
        }
        _ => Err(StorageError::adapter(
            "stored projection event cursor columns are incomplete",
        )),
    }
}

fn load_aggregate_journal(
    connection: &Connection,
    key: &AggregateJournalKey,
) -> Result<Option<LoadedAggregateJournal>, StorageError> {
    let manifest = connection
        .query_row(
            "SELECT manifest FROM aggregate_journals \
             WHERE aggregate_type = ?1 AND aggregate_id = ?2",
            params![key.aggregate_type(), key.aggregate_id()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(sql_error)?;
    let Some(manifest) = manifest else {
        return Ok(None);
    };
    let mut statement = connection
        .prepare(
            "SELECT sequence, digest, payload FROM aggregate_journal_records \
             WHERE aggregate_type = ?1 AND aggregate_id = ?2 ORDER BY sequence",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(params![key.aggregate_type(), key.aggregate_id()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(sql_error)?;
    let records = rows
        .map(|row| {
            let (sequence, digest, payload) = row.map_err(sql_error)?;
            Ok(AggregateJournalRecord {
                sequence: u64::try_from(sequence)
                    .map_err(|_| StorageError::adapter("journal sequence is negative"))?,
                digest,
                payload,
            })
        })
        .collect::<Result<Vec<_>, StorageError>>()?;
    Ok(Some(LoadedAggregateJournal { manifest, records }))
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

fn canonical_control_plane_event_id(value: &str) -> bool {
    value.strip_prefix("evt_").is_some_and(|suffix| {
        (16..=128).contains(&suffix.len())
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

fn portable_event_key(value: &str) -> bool {
    value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'@' | b'-')
    })
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
