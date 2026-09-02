// SPDX-License-Identifier: Apache-2.0

//! Immutable Artifact metadata and content-addressed object storage.
//!
//! [`ArtifactStore`] keeps authority metadata in a Control Plane owned `SQLite`
//! catalog while delegating large bytes to an [`ArtifactObjectStore`]. The
//! object adapter never receives repository scope or Worker credentials.

mod metering;

pub use metering::{
    ArtifactMeteringAttribution, ArtifactStorageOperationKind, ArtifactStorageSourceCursor,
    ArtifactStorageSourceEntry, ArtifactStorageSourceFact, ArtifactStorageSourcePage,
};

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, ExecutionJobId, ExecutionMessageId, FencingToken, LeaseId, RequestId, Sha256Digest,
    WorkerId, WorkerInstanceId, WorkerSessionId,
};

use crate::ReceiptScopeKey;

const CATALOG_FILE_NAME: &str = "artifact-catalog.sqlite3";
const MAX_UNFINISHED_ARTIFACTS_PER_JOB: usize = 4_096;
const CATALOG_STARTUP_LOCK_FILE_NAME: &str = "artifact-catalog.startup.lock";
const CATALOG_SCHEMA_VERSION: i64 = 2;
const MAX_ARTIFACT_BYTES: u64 = 1_099_511_627_776;
const MAX_TEXT_BYTES: usize = 4_096;
static NEXT_TEMPORARY_OBJECT_FILE: AtomicU64 = AtomicU64::new(1);

/// Stable Artifact error categories shared by local and object-store adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactErrorKind {
    InvalidInput,
    NotFound,
    Conflict,
    SequenceGap,
    PermissionDenied,
    Incomplete,
    Retained,
    DigestMismatch,
    Corrupt,
    Adapter,
    Closed,
}

/// Failure returned by Artifact metadata or object storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactError {
    kind: ArtifactErrorKind,
    message: String,
}

impl ArtifactError {
    pub(crate) fn new(kind: ArtifactErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ArtifactErrorKind::InvalidInput, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(ArtifactErrorKind::Conflict, message)
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> Self {
        Self::new(ArtifactErrorKind::Corrupt, message)
    }

    fn digest_mismatch(message: impl Into<String>) -> Self {
        Self::new(ArtifactErrorKind::DigestMismatch, message)
    }

    pub(crate) fn adapter(message: impl Into<String>) -> Self {
        Self::new(ArtifactErrorKind::Adapter, message)
    }

    /// Builds a fixed, secret-safe error at the external object-adapter seam.
    ///
    /// Object adapters deliberately cannot attach endpoint, bucket, key, body,
    /// or provider diagnostics. Domain-only states remain owned by
    /// [`ArtifactStore`] and collapse to an adapter failure at this boundary.
    #[must_use]
    pub fn object_adapter(kind: ArtifactErrorKind) -> Self {
        match kind {
            ArtifactErrorKind::InvalidInput => {
                Self::new(kind, "Artifact object adapter rejected invalid input")
            }
            ArtifactErrorKind::NotFound => {
                Self::new(kind, "Artifact object adapter did not find the object")
            }
            ArtifactErrorKind::Conflict => Self::new(
                kind,
                "Artifact object adapter detected an identity conflict",
            ),
            ArtifactErrorKind::PermissionDenied => {
                Self::new(kind, "Artifact object adapter denied the request")
            }
            ArtifactErrorKind::DigestMismatch => {
                Self::new(kind, "Artifact object adapter rejected corrupt bytes")
            }
            ArtifactErrorKind::Corrupt => {
                Self::new(kind, "Artifact object adapter returned corrupt state")
            }
            ArtifactErrorKind::Adapter
            | ArtifactErrorKind::SequenceGap
            | ArtifactErrorKind::Incomplete
            | ArtifactErrorKind::Retained
            | ArtifactErrorKind::Closed => Self::new(
                ArtifactErrorKind::Adapter,
                "Artifact object adapter is unavailable",
            ),
        }
    }

    fn closed() -> Self {
        Self::new(
            ArtifactErrorKind::Closed,
            "Artifact store is already closed",
        )
    }

    #[must_use]
    pub const fn kind(&self) -> ArtifactErrorKind {
        self.kind
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ArtifactError {}

/// Exact immutable origin of bytes accepted from one fenced `ExecutionJob`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactProvenance {
    execution_job_id: ExecutionJobId,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
}

impl ArtifactProvenance {
    /// Builds the storage identity after the Control Plane has authenticated
    /// the corresponding scheduler lease and Worker session.
    ///
    /// # Errors
    ///
    /// Rejects malformed canonical identities, an invalid attempt, or an
    /// invalid fencing token.
    #[allow(clippy::too_many_arguments)]
    pub fn execution_job(
        execution_job_id: ExecutionJobId,
        attempt: u64,
        lease_id: LeaseId,
        fencing_token: FencingToken,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        worker_session_id: WorkerSessionId,
    ) -> Result<Self, ArtifactError> {
        let provenance = Self {
            execution_job_id,
            attempt,
            lease_id,
            fencing_token,
            worker_id,
            worker_instance_id,
            worker_session_id,
        };
        provenance.validate()?;
        Ok(provenance)
    }

    fn validate(&self) -> Result<(), ArtifactError> {
        canonical_id(&self.execution_job_id.0, "job_", "executionJobId")?;
        canonical_id(&self.lease_id.0, "lse_", "leaseId")?;
        canonical_id(&self.worker_id.0, "wrk_", "workerId")?;
        canonical_id(&self.worker_instance_id.0, "wki_", "workerInstanceId")?;
        canonical_id(&self.worker_session_id.0, "wsn_", "workerSessionId")?;
        if self.attempt == 0 || self.attempt > i64::MAX as u64 {
            return Err(ArtifactError::invalid(
                "Artifact provenance attempt is outside the supported range",
            ));
        }
        if self.fencing_token.0.is_empty()
            || self.fencing_token.0.len() > 20
            || self.fencing_token.0.starts_with('0')
            || !self
                .fencing_token
                .0
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            return Err(ArtifactError::invalid(
                "Artifact provenance fencing token is invalid",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub const fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    #[must_use]
    pub const fn attempt(&self) -> u64 {
        self.attempt
    }

    #[must_use]
    pub const fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }

    #[must_use]
    pub const fn fencing_token(&self) -> &FencingToken {
        &self.fencing_token
    }

    #[must_use]
    pub const fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    #[must_use]
    pub const fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.worker_instance_id
    }

    #[must_use]
    pub const fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }
}

/// Deletion hold saved with Artifact metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactRetention {
    UntilMillis(u64),
    Indefinite,
}

/// Immutable metadata accepted before the first Artifact chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactOpen {
    scope_key: ReceiptScopeKey,
    message_id: ExecutionMessageId,
    request_id: RequestId,
    artifact_id: ArtifactId,
    kind: String,
    media_type: String,
    digest: Sha256Digest,
    size_bytes: u64,
    file_name: Option<String>,
    provenance: ArtifactProvenance,
    metering_attribution: ArtifactMeteringAttribution,
    retention: ArtifactRetention,
    created_at_millis: u64,
}

impl ArtifactOpen {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        scope_key: ReceiptScopeKey,
        message_id: ExecutionMessageId,
        request_id: RequestId,
        artifact_id: ArtifactId,
        kind: impl Into<String>,
        media_type: impl Into<String>,
        digest: Sha256Digest,
        size_bytes: u64,
        file_name: Option<String>,
        provenance: ArtifactProvenance,
        metering_attribution: ArtifactMeteringAttribution,
        retention: ArtifactRetention,
        created_at_millis: u64,
    ) -> Self {
        Self {
            scope_key,
            message_id,
            request_id,
            artifact_id,
            kind: kind.into(),
            media_type: media_type.into(),
            digest,
            size_bytes,
            file_name,
            provenance,
            metering_attribution,
            retention,
            created_at_millis,
        }
    }

    /// Returns the immutable Artifact identity sealed before storage admission.
    #[must_use]
    pub const fn artifact_id(&self) -> &ArtifactId {
        &self.artifact_id
    }

    /// Returns the durable open request identity used by storage metering.
    #[must_use]
    pub const fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Returns the exact byte count promised before the object write starts.
    #[must_use]
    pub const fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    /// Returns the trusted Control Plane attribution frozen with this open.
    #[must_use]
    pub const fn metering_attribution(&self) -> &ArtifactMeteringAttribution {
        &self.metering_attribution
    }

    /// Returns the validated open time in Unix milliseconds.
    #[must_use]
    pub const fn created_at_millis(&self) -> u64 {
        self.created_at_millis
    }

    fn validate(&self) -> Result<(), ArtifactError> {
        canonical_id(&self.message_id.0, "xmsg_", "messageId")?;
        canonical_id(&self.request_id.0, "req_", "requestId")?;
        canonical_id(&self.artifact_id.0, "art_", "artifactId")?;
        self.provenance.validate()?;
        self.metering_attribution.validate()?;
        sha256_hex(&self.digest)?;
        bounded_text(&self.kind, "Artifact kind", 120)?;
        bounded_text(&self.media_type, "Artifact media type", 200)?;
        if self.size_bytes > MAX_ARTIFACT_BYTES {
            return Err(ArtifactError::invalid(
                "Artifact size exceeds the supported maximum",
            ));
        }
        if let Some(file_name) = &self.file_name {
            bounded_text(file_name, "Artifact file name", 255)?;
            if file_name.contains(['/', '\\']) || matches!(file_name.as_str(), "." | "..") {
                return Err(ArtifactError::invalid(
                    "Artifact file name must not contain a path",
                ));
            }
        }
        if self.created_at_millis == 0 {
            return Err(ArtifactError::invalid(
                "Artifact creation time must be positive",
            ));
        }
        if let ArtifactRetention::UntilMillis(until) = self.retention
            && until < self.created_at_millis
        {
            return Err(ArtifactError::invalid(
                "Artifact retention cannot end before creation",
            ));
        }
        Ok(())
    }
}

/// One ordered chunk after transport decoding and per-chunk digest checking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactChunk {
    scope_key: ReceiptScopeKey,
    message_id: ExecutionMessageId,
    artifact_id: ArtifactId,
    provenance: ArtifactProvenance,
    sent_at_millis: u64,
    sequence: u64,
    content_type: String,
    digest: Sha256Digest,
    bytes: Vec<u8>,
    is_final: bool,
}

impl ArtifactChunk {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        scope_key: ReceiptScopeKey,
        message_id: ExecutionMessageId,
        artifact_id: ArtifactId,
        provenance: ArtifactProvenance,
        sent_at_millis: u64,
        sequence: u64,
        content_type: impl Into<String>,
        digest: Sha256Digest,
        bytes: Vec<u8>,
        is_final: bool,
    ) -> Self {
        Self {
            scope_key,
            message_id,
            artifact_id,
            provenance,
            sent_at_millis,
            sequence,
            content_type: content_type.into(),
            digest,
            bytes,
            is_final,
        }
    }

    fn validate(&self) -> Result<(), ArtifactError> {
        canonical_id(&self.message_id.0, "xmsg_", "messageId")?;
        canonical_id(&self.artifact_id.0, "art_", "artifactId")?;
        self.provenance.validate()?;
        if self.sent_at_millis == 0 {
            return Err(ArtifactError::invalid(
                "Artifact chunk sent time must be positive",
            ));
        }
        if self.sequence == 0 || self.sequence > i64::MAX as u64 {
            return Err(ArtifactError::invalid(
                "Artifact chunk sequence is outside the supported range",
            ));
        }
        bounded_text(&self.content_type, "Artifact chunk content type", 200)?;
        let expected = sha256_hex(&self.digest)?;
        let actual = format!("{:x}", Sha256::digest(&self.bytes));
        if expected != actual {
            return Err(ArtifactError::digest_mismatch(
                "Artifact chunk bytes do not match payloadDigest",
            ));
        }
        Ok(())
    }
}

/// Exact read authority. Scope, Artifact identity, content digest, and origin
/// all have to match the immutable catalog row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactAccess {
    scope_key: ReceiptScopeKey,
    artifact_id: ArtifactId,
    digest: Sha256Digest,
    provenance: ArtifactProvenance,
}

impl ArtifactAccess {
    #[must_use]
    pub fn new(
        scope_key: ReceiptScopeKey,
        artifact_id: ArtifactId,
        digest: Sha256Digest,
        provenance: ArtifactProvenance,
    ) -> Self {
        Self {
            scope_key,
            artifact_id,
            digest,
            provenance,
        }
    }
}

/// Immutable Artifact metadata returned through the storage interface.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRecord {
    open: ArtifactOpen,
    acknowledged_sequence: u64,
    complete: bool,
    deleted_at_millis: Option<u64>,
}

impl ArtifactRecord {
    #[must_use]
    pub const fn artifact_id(&self) -> &ArtifactId {
        &self.open.artifact_id
    }

    #[must_use]
    pub fn kind(&self) -> &str {
        &self.open.kind
    }

    #[must_use]
    pub fn media_type(&self) -> &str {
        &self.open.media_type
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.open.digest
    }

    #[must_use]
    pub const fn size_bytes(&self) -> u64 {
        self.open.size_bytes
    }

    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        self.open.file_name.as_deref()
    }

    #[must_use]
    pub const fn provenance(&self) -> &ArtifactProvenance {
        &self.open.provenance
    }

    #[must_use]
    pub const fn metering_attribution(&self) -> &ArtifactMeteringAttribution {
        &self.open.metering_attribution
    }

    #[must_use]
    pub const fn retention(&self) -> ArtifactRetention {
        self.open.retention
    }

    #[must_use]
    pub const fn created_at_millis(&self) -> u64 {
        self.open.created_at_millis
    }

    #[must_use]
    pub const fn acknowledged_sequence(&self) -> u64 {
        self.acknowledged_sequence
    }

    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    #[must_use]
    pub const fn deleted_at_millis(&self) -> Option<u64> {
        self.deleted_at_millis
    }
}

/// Result of an open or chunk write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactWriteReceipt {
    record: ArtifactRecord,
    duplicate: bool,
}

impl ArtifactWriteReceipt {
    #[must_use]
    pub const fn acknowledged_sequence(&self) -> u64 {
        self.record.acknowledged_sequence
    }

    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.record.complete
    }

    #[must_use]
    pub const fn is_duplicate(&self) -> bool {
        self.duplicate
    }

    #[must_use]
    pub const fn record(&self) -> &ArtifactRecord {
        &self.record
    }
}

/// Complete content plus its catalog metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactObject {
    metadata: ArtifactRecord,
    bytes: Vec<u8>,
}

impl ArtifactObject {
    #[must_use]
    pub const fn metadata(&self) -> &ArtifactRecord {
        &self.metadata
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Byte-storage seam implemented by controlled local files and object stores.
pub trait ArtifactObjectStore: Send {
    /// Stores one immutable upload chunk. Exact repeats are accepted.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, changed repeats, corrupt bytes, or an
    /// adapter write failure.
    fn put_chunk(
        &mut self,
        artifact_id: &ArtifactId,
        sequence: u64,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ArtifactError>;

    /// Joins contiguous chunks into one digest-addressed immutable object.
    ///
    /// # Errors
    ///
    /// Rejects missing chunks, an aggregate digest/size mismatch, or an
    /// adapter finalization failure.
    fn finalize(
        &mut self,
        artifact_id: &ArtifactId,
        last_sequence: u64,
        digest: &Sha256Digest,
        size_bytes: u64,
    ) -> Result<(), ArtifactError>;

    /// Reads one object by content digest.
    ///
    /// # Errors
    ///
    /// Rejects malformed content addresses or an adapter read failure.
    fn read(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, ArtifactError>;

    /// Deletes one unreferenced content object. Missing objects are idempotent.
    ///
    /// # Errors
    ///
    /// Rejects malformed content addresses or an adapter deletion failure.
    fn delete(&mut self, digest: &Sha256Digest) -> Result<(), ArtifactError>;

    /// Releases adapter resources.
    ///
    /// # Errors
    ///
    /// Returns an adapter-specific deterministic close failure.
    fn close(self: Box<Self>) -> Result<(), ArtifactError> {
        Ok(())
    }
}

/// Deep Artifact module: `SQLite` authority metadata plus swappable large-object bytes.
pub struct ArtifactStore {
    catalog: Option<Connection>,
    objects: Option<Box<dyn ArtifactObjectStore>>,
}

impl ArtifactStore {
    /// Opens the Control Plane metadata catalog with the selected object adapter.
    ///
    /// # Errors
    ///
    /// Returns an adapter error if the directory, `SQLite` catalog, migration,
    /// or object adapter cannot be prepared.
    pub fn open(
        data_directory: impl AsRef<Path>,
        objects: Box<dyn ArtifactObjectStore>,
    ) -> Result<Self, ArtifactError> {
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory).map_err(io_error)?;
        let startup_lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(data_directory.join(CATALOG_STARTUP_LOCK_FILE_NAME))
            .map_err(io_error)?;
        startup_lock.lock().map_err(io_error)?;
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX;
        let mut catalog =
            Connection::open_with_flags(data_directory.join(CATALOG_FILE_NAME), flags)
                .map_err(sql_error)?;
        catalog
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sql_error)?;
        catalog
            .pragma_update(None, "foreign_keys", true)
            .map_err(sql_error)?;
        catalog
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_error)?;
        catalog
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql_error)?;
        migrate_catalog(&mut catalog)?;
        startup_lock.unlock().map_err(io_error)?;
        Ok(Self {
            catalog: Some(catalog),
            objects: Some(objects),
        })
    }

    /// Opens or exactly replays immutable Artifact metadata.
    ///
    /// # Errors
    ///
    /// Rejects malformed metadata or any attempt to reuse an `ArtifactId` with
    /// another scope, digest, provenance, retention rule, or descriptor.
    pub fn open_artifact(
        &mut self,
        open: ArtifactOpen,
    ) -> Result<ArtifactWriteReceipt, ArtifactError> {
        open.validate()?;
        let catalog = self.catalog_mut()?;
        let transaction = catalog
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(existing) = replay_open_identity(&transaction, &open)? {
            transaction.commit().map_err(sql_error)?;
            return Ok(ArtifactWriteReceipt {
                record: existing,
                duplicate: true,
            });
        }
        if let Some(existing) = load_record(&transaction, &open.artifact_id)? {
            if existing.deleted_at_millis.is_some() {
                return Err(ArtifactError::conflict(
                    "deleted ArtifactId is permanently tombstoned",
                ));
            }
            if existing.open.scope_key != open.scope_key {
                return Err(ArtifactError::new(
                    ArtifactErrorKind::PermissionDenied,
                    "ArtifactId belongs to another repository scope",
                ));
            }
            if existing.open != open {
                return Err(ArtifactError::conflict(
                    "ArtifactId is already bound to different immutable metadata",
                ));
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(ArtifactWriteReceipt {
                record: existing,
                duplicate: true,
            });
        }
        insert_open(&transaction, &open)?;
        let record = ArtifactRecord {
            open,
            acknowledged_sequence: 0,
            complete: false,
            deleted_at_millis: None,
        };
        transaction.commit().map_err(sql_error)?;
        Ok(ArtifactWriteReceipt {
            record,
            duplicate: false,
        })
    }

    /// Looks up one durable open acknowledgement without creating metadata.
    /// Exact replays remain available after the originating `StageRun` settles;
    /// a changed reuse still fails closed.
    ///
    /// # Errors
    ///
    /// Rejects malformed input, foreign scope, changed identity reuse, or a
    /// corrupt catalog row.
    pub fn replay_open(
        &self,
        open: &ArtifactOpen,
    ) -> Result<Option<ArtifactWriteReceipt>, ArtifactError> {
        open.validate()?;
        Ok(
            replay_open_identity(self.catalog_ref()?, open)?.map(|record| ArtifactWriteReceipt {
                record,
                duplicate: true,
            }),
        )
    }

    /// Stores one ordered chunk and finalizes content only after the exact
    /// aggregate size and digest match the open descriptor.
    ///
    /// # Errors
    ///
    /// Rejects gaps, changed repeats, foreign scope, writes after completion,
    /// per-chunk corruption, or an aggregate size/digest mismatch.
    pub fn append_chunk(
        &mut self,
        chunk: &ArtifactChunk,
    ) -> Result<ArtifactWriteReceipt, ArtifactError> {
        chunk.validate()?;
        if let Some(receipt) = self.replay_chunk_message(chunk)? {
            return Ok(receipt);
        }
        let current = self.load_authorized(&chunk.scope_key, &chunk.artifact_id)?;
        require_chunk_provenance(&current, chunk)?;
        if let Some(receipt) = self.replay_prior_sequence(&current, chunk)? {
            return Ok(receipt);
        }
        self.validate_next_chunk(&current, chunk)?;
        self.write_chunk_object(&current, chunk)?;
        self.commit_chunk_metadata(current, chunk)
    }

    /// Looks up one durable chunk acknowledgement without accepting new bytes.
    ///
    /// # Errors
    ///
    /// Rejects malformed input, foreign provenance, or changed message reuse.
    pub fn replay_chunk(
        &self,
        chunk: &ArtifactChunk,
    ) -> Result<Option<ArtifactWriteReceipt>, ArtifactError> {
        chunk.validate()?;
        self.replay_chunk_message(chunk)
    }

    fn replay_chunk_message(
        &self,
        chunk: &ArtifactChunk,
    ) -> Result<Option<ArtifactWriteReceipt>, ArtifactError> {
        if let Some(stored) = self.load_chunk_by_message_id(&chunk.message_id)? {
            let record = self.load_authorized(&chunk.scope_key, &stored.artifact_id)?;
            require_chunk_provenance(&record, chunk)?;
            if message_chunk_is_exact(&stored, chunk) {
                return Ok(Some(ArtifactWriteReceipt {
                    record,
                    duplicate: true,
                }));
            }
            return Err(ArtifactError::conflict(
                "Artifact chunk message identity was reused with different content",
            ));
        }
        Ok(None)
    }

    fn replay_prior_sequence(
        &self,
        current: &ArtifactRecord,
        chunk: &ArtifactChunk,
    ) -> Result<Option<ArtifactWriteReceipt>, ArtifactError> {
        if current.complete || chunk.sequence <= current.acknowledged_sequence {
            let exact = self
                .load_chunk(&chunk.artifact_id, chunk.sequence)?
                .is_some_and(|stored| stored_chunk_is_exact(&stored, chunk));
            if exact {
                return Ok(Some(ArtifactWriteReceipt {
                    record: current.clone(),
                    duplicate: true,
                }));
            }
            return Err(ArtifactError::conflict(if current.complete {
                "completed Artifact content is immutable"
            } else {
                "Artifact chunk sequence was reused with different content"
            }));
        }
        Ok(None)
    }

    fn validate_next_chunk(
        &self,
        current: &ArtifactRecord,
        chunk: &ArtifactChunk,
    ) -> Result<(), ArtifactError> {
        if chunk.sequence != current.acknowledged_sequence + 1 {
            return Err(ArtifactError::new(
                ArtifactErrorKind::SequenceGap,
                format!(
                    "Artifact chunk gap: expected sequence {}, received {}",
                    current.acknowledged_sequence + 1,
                    chunk.sequence
                ),
            ));
        }
        let prior_size = self.received_size(&chunk.artifact_id)?;
        let received_size = prior_size
            .checked_add(chunk.bytes.len() as u64)
            .ok_or_else(|| ArtifactError::invalid("Artifact received size overflow"))?;
        if received_size > current.open.size_bytes
            || (chunk.is_final && received_size != current.open.size_bytes)
            || (!chunk.is_final && received_size >= current.open.size_bytes)
        {
            return Err(ArtifactError::conflict(
                "Artifact chunks do not match the declared total size",
            ));
        }
        Ok(())
    }

    fn write_chunk_object(
        &mut self,
        current: &ArtifactRecord,
        chunk: &ArtifactChunk,
    ) -> Result<(), ArtifactError> {
        self.objects_mut()?.put_chunk(
            &chunk.artifact_id,
            chunk.sequence,
            &chunk.digest,
            &chunk.bytes,
        )?;
        if chunk.is_final {
            self.objects_mut()?.finalize(
                &chunk.artifact_id,
                chunk.sequence,
                &current.open.digest,
                current.open.size_bytes,
            )?;
        }
        Ok(())
    }

    fn commit_chunk_metadata(
        &mut self,
        current: ArtifactRecord,
        chunk: &ArtifactChunk,
    ) -> Result<ArtifactWriteReceipt, ArtifactError> {
        let catalog = self.catalog_mut()?;
        let transaction = catalog
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let latest = load_record(&transaction, &chunk.artifact_id)?.ok_or_else(|| {
            ArtifactError::new(ArtifactErrorKind::NotFound, "Artifact metadata is missing")
        })?;
        if let Some(stored) = load_message_chunk(&transaction, &chunk.message_id)? {
            let stored_record =
                load_record(&transaction, &stored.artifact_id)?.ok_or_else(|| {
                    ArtifactError::corrupt(
                        "Artifact chunk message points to missing Artifact metadata",
                    )
                })?;
            if stored_record.open.scope_key != chunk.scope_key {
                return Err(ArtifactError::new(
                    ArtifactErrorKind::PermissionDenied,
                    "Artifact chunk message identity belongs to another repository scope",
                ));
            }
            require_chunk_provenance(&stored_record, chunk)?;
            if message_chunk_is_exact(&stored, chunk) {
                if stored.chunk.is_final {
                    metering::require_complete_source(&transaction, &stored_record)?;
                }
                transaction.commit().map_err(sql_error)?;
                return Ok(ArtifactWriteReceipt {
                    record: stored_record,
                    duplicate: true,
                });
            }
            return Err(ArtifactError::conflict(
                "Artifact chunk message identity was reused with different content",
            ));
        }
        if latest.open.scope_key != chunk.scope_key
            || latest.open.provenance != chunk.provenance
            || latest.acknowledged_sequence + 1 != chunk.sequence
            || latest.complete
        {
            return Err(ArtifactError::conflict(
                "Artifact metadata changed while appending a chunk",
            ));
        }
        if chunk.is_final {
            let _ = metering::insert_final_source(&transaction, &latest, chunk)?;
        }
        transaction
            .execute(
                "INSERT INTO artifact_chunks
                 (artifact_id, message_id, sent_at_millis, sequence, content_type,
                  digest, size_bytes, is_final)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    chunk.artifact_id.0,
                    chunk.message_id.0,
                    i64_value(chunk.sent_at_millis, "Artifact chunk sent timestamp")?,
                    i64_value(chunk.sequence, "Artifact chunk sequence")?,
                    chunk.content_type,
                    chunk.digest.0,
                    i64_value(chunk.bytes.len() as u64, "Artifact chunk size")?,
                    i64::from(chunk.is_final),
                ],
            )
            .map_err(sql_error)?;
        transaction
            .execute(
                "UPDATE artifacts SET acknowledged_sequence = ?2, complete = ?3
                 WHERE artifact_id = ?1",
                params![
                    chunk.artifact_id.0,
                    i64_value(chunk.sequence, "Artifact chunk sequence")?,
                    i64::from(chunk.is_final),
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(ArtifactWriteReceipt {
            record: ArtifactRecord {
                open: current.open,
                acknowledged_sequence: chunk.sequence,
                complete: chunk.is_final,
                deleted_at_millis: None,
            },
            duplicate: false,
        })
    }

    /// Reads and re-hashes one exact complete object.
    ///
    /// # Errors
    ///
    /// Rejects foreign scope or provenance, a changed digest, incomplete or
    /// deleted metadata, missing bytes, and any object size/hash corruption.
    pub fn read_exact(&self, access: &ArtifactAccess) -> Result<ArtifactObject, ArtifactError> {
        canonical_id(&access.artifact_id.0, "art_", "artifactId")?;
        sha256_hex(&access.digest)?;
        let record = self.load_authorized(&access.scope_key, &access.artifact_id)?;
        if record.open.digest != access.digest || record.open.provenance != access.provenance {
            return Err(ArtifactError::new(
                ArtifactErrorKind::PermissionDenied,
                "Artifact access does not match its immutable digest and provenance",
            ));
        }
        if !record.complete {
            return Err(ArtifactError::new(
                ArtifactErrorKind::Incomplete,
                "Artifact upload is not complete",
            ));
        }
        let bytes = self
            .objects_ref()?
            .read(&record.open.digest)?
            .ok_or_else(|| ArtifactError::corrupt("Artifact content object is missing"))?;
        validate_complete_bytes(&record.open.digest, record.open.size_bytes, &bytes)?;
        Ok(ArtifactObject {
            metadata: record,
            bytes,
        })
    }

    /// Reconstructs the durable write receipt for one complete Artifact.
    ///
    /// This is the receipt-first bridge used by candidate Git retention after
    /// an acknowledgement response is lost.  It reads the catalog authority
    /// with the exact scope and provenance, and never trusts a caller-provided
    /// descriptor or sequence.  The returned receipt is marked as a replay so
    /// callers can distinguish recovery from a newly accepted write.
    ///
    /// # Errors
    ///
    /// Rejects a missing, foreign, deleted, or incomplete Artifact, or a
    /// closed/corrupt catalog.
    pub fn complete_write_receipt(
        &self,
        scope_key: &ReceiptScopeKey,
        artifact_id: &ArtifactId,
        provenance: &ArtifactProvenance,
    ) -> Result<ArtifactWriteReceipt, ArtifactError> {
        canonical_id(&artifact_id.0, "art_", "artifactId")?;
        let record = self.load_authorized(scope_key, artifact_id)?;
        if &record.open.provenance != provenance {
            return Err(ArtifactError::new(
                ArtifactErrorKind::PermissionDenied,
                "Artifact receipt provenance does not match its immutable authority",
            ));
        }
        if !record.complete {
            return Err(ArtifactError::new(
                ArtifactErrorKind::Incomplete,
                "Artifact upload is not complete",
            ));
        }
        Ok(ArtifactWriteReceipt {
            record,
            duplicate: true,
        })
    }

    /// Returns the highest contiguous sequence accepted for one Artifact.
    ///
    /// This is the transport recovery cursor used after a sequence gap or a
    /// changed repeat. It requires the same exact repository scope as writes.
    ///
    /// # Errors
    ///
    /// Rejects a missing, deleted, or foreign Artifact identity.
    pub fn acknowledged_sequence(
        &self,
        scope_key: &ReceiptScopeKey,
        artifact_id: &ArtifactId,
    ) -> Result<u64, ArtifactError> {
        Ok(self
            .load_authorized(scope_key, artifact_id)?
            .acknowledged_sequence)
    }

    /// Reads one bounded page of immutable completed-storage sources.
    ///
    /// The first page freezes a sequence upper bound, so concurrent Artifact
    /// completions are visible only when a new scan begins.
    ///
    /// # Errors
    ///
    /// Rejects invalid limits/cursors, incomplete or mismatched source rows,
    /// corrupt Artifact metadata, and unavailable catalog storage.
    pub fn scan_storage_sources(
        &self,
        cursor: Option<&ArtifactStorageSourceCursor>,
        limit: u64,
    ) -> Result<ArtifactStorageSourcePage, ArtifactError> {
        metering::scan(self.catalog_ref()?, cursor, limit)
    }

    /// Loads the one immutable storage-finalization source for an Artifact.
    ///
    /// A completed Artifact must have exactly one source. An incomplete
    /// Artifact has no source and therefore cannot be enterprise-settled.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, source/catalog mismatches, and unavailable
    /// catalog storage.
    pub fn storage_source_for_artifact(
        &self,
        artifact_id: &ArtifactId,
    ) -> Result<Option<ArtifactStorageSourceEntry>, ArtifactError> {
        metering::load_for_artifact(self.catalog_ref()?, artifact_id)
    }

    /// Loads the bounded immutable opens that still reserve storage for one Job.
    ///
    /// The catalog, rather than a terminal Worker message, is the authority for
    /// the open request, expected bytes, attribution, and source seal. Completed
    /// Artifacts are omitted because their immutable Usage source must settle
    /// rather than release the reservation.
    ///
    /// # Errors
    ///
    /// Rejects malformed Job identity, corrupt/foreign catalog rows, an
    /// unavailable catalog, or more than the supported unfinished bound.
    pub fn unfinished_quota_opens_for_job(
        &self,
        execution_job_id: &ExecutionJobId,
    ) -> Result<Vec<ArtifactOpen>, ArtifactError> {
        canonical_id(&execution_job_id.0, "job_", "executionJobId")?;
        unfinished_quota_opens_for_job(self.catalog_ref()?, execution_job_id)
    }

    /// Tombstones one exact Artifact after its retention hold ends. Content is
    /// removed only when no other live Artifact metadata references the same
    /// digest.
    ///
    /// # Errors
    ///
    /// Rejects foreign access, indefinite or unexpired retention, incomplete
    /// uploads, and malformed deletion time. Exact repeated deletion is safe.
    pub fn delete(
        &mut self,
        access: &ArtifactAccess,
        deleted_at_millis: u64,
    ) -> Result<(), ArtifactError> {
        if deleted_at_millis == 0 {
            return Err(ArtifactError::invalid(
                "Artifact deletion time must be positive",
            ));
        }
        canonical_id(&access.artifact_id.0, "art_", "artifactId")?;
        sha256_hex(&access.digest)?;
        let catalog = self.catalog_mut()?;
        let transaction = catalog
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let record = load_record(&transaction, &access.artifact_id)?.ok_or_else(|| {
            ArtifactError::new(ArtifactErrorKind::NotFound, "Artifact metadata is missing")
        })?;
        if record.open.scope_key != access.scope_key
            || record.open.digest != access.digest
            || record.open.provenance != access.provenance
        {
            return Err(ArtifactError::new(
                ArtifactErrorKind::PermissionDenied,
                "Artifact deletion does not match its immutable scope, digest, and provenance",
            ));
        }
        if !record.complete {
            return Err(ArtifactError::new(
                ArtifactErrorKind::Incomplete,
                "incomplete Artifact cannot be deleted through the retention path",
            ));
        }
        if record.deleted_at_millis.is_none() {
            match record.open.retention {
                ArtifactRetention::Indefinite => {
                    return Err(ArtifactError::new(
                        ArtifactErrorKind::Retained,
                        "Artifact has indefinite retention",
                    ));
                }
                ArtifactRetention::UntilMillis(until) if deleted_at_millis < until => {
                    return Err(ArtifactError::new(
                        ArtifactErrorKind::Retained,
                        "Artifact retention has not expired",
                    ));
                }
                ArtifactRetention::UntilMillis(_) => {}
            }
            transaction
                .execute(
                    "UPDATE artifacts SET deleted_at_millis = ?2 WHERE artifact_id = ?1",
                    params![
                        access.artifact_id.0,
                        i64_value(deleted_at_millis, "Artifact deletion timestamp")?
                    ],
                )
                .map_err(sql_error)?;
        }
        transaction.commit().map_err(sql_error)?;
        self.delete_object_if_unreferenced(&access.digest)
    }

    /// Closes both the object adapter and metadata catalog.
    ///
    /// # Errors
    ///
    /// Attempts both close operations and reports all failures.
    pub fn close(mut self) -> Result<(), ArtifactError> {
        let mut failures = Vec::new();
        if let Some(objects) = self.objects.take()
            && let Err(error) = objects.close()
        {
            failures.push(format!("object adapter close failed: {error}"));
        }
        if let Some(catalog) = self.catalog.take()
            && let Err((_, error)) = catalog.close()
        {
            failures.push(format!("Artifact catalog close failed: {error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ArtifactError::adapter(failures.join("; ")))
        }
    }

    fn catalog_ref(&self) -> Result<&Connection, ArtifactError> {
        self.catalog.as_ref().ok_or_else(ArtifactError::closed)
    }

    fn catalog_mut(&mut self) -> Result<&mut Connection, ArtifactError> {
        self.catalog.as_mut().ok_or_else(ArtifactError::closed)
    }

    fn objects_ref(&self) -> Result<&dyn ArtifactObjectStore, ArtifactError> {
        self.objects.as_deref().ok_or_else(ArtifactError::closed)
    }

    fn objects_mut(&mut self) -> Result<&mut (dyn ArtifactObjectStore + 'static), ArtifactError> {
        self.objects
            .as_deref_mut()
            .ok_or_else(ArtifactError::closed)
    }

    fn load_authorized(
        &self,
        scope_key: &ReceiptScopeKey,
        artifact_id: &ArtifactId,
    ) -> Result<ArtifactRecord, ArtifactError> {
        let record = load_record(self.catalog_ref()?, artifact_id)?.ok_or_else(|| {
            ArtifactError::new(ArtifactErrorKind::NotFound, "Artifact metadata is missing")
        })?;
        if record.deleted_at_millis.is_some() {
            return Err(ArtifactError::new(
                ArtifactErrorKind::NotFound,
                "Artifact metadata is deleted",
            ));
        }
        if &record.open.scope_key != scope_key {
            return Err(ArtifactError::new(
                ArtifactErrorKind::PermissionDenied,
                "Artifact belongs to another repository scope",
            ));
        }
        Ok(record)
    }

    fn load_chunk(
        &self,
        artifact_id: &ArtifactId,
        sequence: u64,
    ) -> Result<Option<StoredChunk>, ArtifactError> {
        self.catalog_ref()?
            .query_row(
                "SELECT message_id, sent_at_millis, content_type, digest, size_bytes, is_final
                 FROM artifact_chunks
                 WHERE artifact_id = ?1 AND sequence = ?2",
                params![
                    artifact_id.0,
                    i64_value(sequence, "Artifact chunk sequence")?
                ],
                |row| {
                    Ok(StoredChunk {
                        message_id: ExecutionMessageId(row.get(0)?),
                        sent_at_millis: row
                            .get::<_, i64>(1)?
                            .try_into()
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, i64::MIN))?,
                        content_type: row.get(2)?,
                        digest: Sha256Digest(row.get(3)?),
                        size_bytes: row
                            .get::<_, i64>(4)?
                            .try_into()
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(4, i64::MIN))?,
                        is_final: row.get::<_, i64>(5)? == 1,
                    })
                },
            )
            .optional()
            .map_err(sql_error)
    }

    fn load_chunk_by_message_id(
        &self,
        message_id: &ExecutionMessageId,
    ) -> Result<Option<StoredMessageChunk>, ArtifactError> {
        load_message_chunk(self.catalog_ref()?, message_id)
    }

    fn received_size(&self, artifact_id: &ArtifactId) -> Result<u64, ArtifactError> {
        let value = self
            .catalog_ref()?
            .query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM artifact_chunks WHERE artifact_id = ?1",
                [&artifact_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error)?;
        value
            .try_into()
            .map_err(|_| ArtifactError::corrupt("Artifact received size is invalid"))
    }

    fn delete_object_if_unreferenced(
        &mut self,
        digest: &Sha256Digest,
    ) -> Result<(), ArtifactError> {
        let (catalog, objects) = (&mut self.catalog, &mut self.objects);
        let catalog = catalog.as_mut().ok_or_else(ArtifactError::closed)?;
        let transaction = catalog
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let live_references = transaction
            .query_row(
                "SELECT COUNT(*) FROM artifacts WHERE digest = ?1 AND deleted_at_millis IS NULL",
                [&digest.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql_error)?;
        if live_references == 0 {
            objects
                .as_deref_mut()
                .ok_or_else(ArtifactError::closed)?
                .delete(digest)?;
        }
        transaction.commit().map_err(sql_error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredChunk {
    message_id: ExecutionMessageId,
    sent_at_millis: u64,
    content_type: String,
    digest: Sha256Digest,
    size_bytes: u64,
    is_final: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredMessageChunk {
    artifact_id: ArtifactId,
    sequence: u64,
    chunk: StoredChunk,
}

fn stored_chunk_is_exact(stored: &StoredChunk, chunk: &ArtifactChunk) -> bool {
    stored.message_id == chunk.message_id
        && stored.sent_at_millis == chunk.sent_at_millis
        && stored.content_type == chunk.content_type
        && stored.digest == chunk.digest
        && stored.size_bytes == chunk.bytes.len() as u64
        && stored.is_final == chunk.is_final
}

fn message_chunk_is_exact(stored: &StoredMessageChunk, chunk: &ArtifactChunk) -> bool {
    stored.artifact_id == chunk.artifact_id
        && stored.sequence == chunk.sequence
        && stored_chunk_is_exact(&stored.chunk, chunk)
}

fn require_chunk_provenance(
    record: &ArtifactRecord,
    chunk: &ArtifactChunk,
) -> Result<(), ArtifactError> {
    if record.open.provenance != chunk.provenance {
        return Err(ArtifactError::new(
            ArtifactErrorKind::PermissionDenied,
            "Artifact chunk does not match the immutable ExecutionJob provenance",
        ));
    }
    Ok(())
}

fn load_message_chunk(
    connection: &Connection,
    message_id: &ExecutionMessageId,
) -> Result<Option<StoredMessageChunk>, ArtifactError> {
    connection
        .query_row(
            "SELECT artifact_id, sequence, sent_at_millis, content_type, digest, size_bytes, is_final
             FROM artifact_chunks WHERE message_id = ?1",
            [&message_id.0],
            |row| {
                let sequence = row.get::<_, i64>(1)?;
                let sent_at_millis = row.get::<_, i64>(2)?;
                let size_bytes = row.get::<_, i64>(5)?;
                Ok(StoredMessageChunk {
                    artifact_id: ArtifactId(row.get(0)?),
                    sequence: sequence
                        .try_into()
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, sequence))?,
                    chunk: StoredChunk {
                        message_id: message_id.clone(),
                        sent_at_millis: sent_at_millis.try_into().map_err(|_| {
                            rusqlite::Error::IntegralValueOutOfRange(2, sent_at_millis)
                        })?,
                        content_type: row.get(3)?,
                        digest: Sha256Digest(row.get(4)?),
                        size_bytes: size_bytes
                            .try_into()
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(5, size_bytes))?,
                        is_final: row.get::<_, i64>(6)? == 1,
                    },
                })
            },
        )
        .optional()
        .map_err(sql_error)
}

/// Controlled local filesystem object adapter.
pub struct LocalArtifactObjectStore {
    root: PathBuf,
}

impl LocalArtifactObjectStore {
    /// Opens a root owned by the adapter. Callers never provide object keys or
    /// paths after construction.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the root cannot be created or normalized.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        let root = root.as_ref();
        fs::create_dir_all(root.join("objects/sha256")).map_err(io_error)?;
        fs::create_dir_all(root.join("uploads")).map_err(io_error)?;
        let root = fs::canonicalize(root).map_err(io_error)?;
        Ok(Self { root })
    }

    fn upload_directory(&self, artifact_id: &ArtifactId) -> Result<PathBuf, ArtifactError> {
        canonical_id(&artifact_id.0, "art_", "artifactId")?;
        Ok(self.root.join("uploads").join(&artifact_id.0))
    }

    fn chunk_path(
        &self,
        artifact_id: &ArtifactId,
        sequence: u64,
    ) -> Result<PathBuf, ArtifactError> {
        Ok(self
            .upload_directory(artifact_id)?
            .join(format!("{sequence:020}.chunk")))
    }

    fn object_path(&self, digest: &Sha256Digest) -> Result<PathBuf, ArtifactError> {
        let hex = sha256_hex(digest)?;
        Ok(self
            .root
            .join("objects/sha256")
            .join(&hex[..2])
            .join(&hex[2..]))
    }
}

impl ArtifactObjectStore for LocalArtifactObjectStore {
    fn put_chunk(
        &mut self,
        artifact_id: &ArtifactId,
        sequence: u64,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ArtifactError> {
        let expected = sha256_hex(digest)?;
        if format!("{:x}", Sha256::digest(bytes)) != expected {
            return Err(ArtifactError::digest_mismatch(
                "Artifact object adapter rejected a corrupt chunk",
            ));
        }
        let path = self.chunk_path(artifact_id, sequence)?;
        if path.exists() {
            let existing = fs::read(&path).map_err(io_error)?;
            if existing == bytes {
                return Ok(());
            }
            return Err(ArtifactError::conflict(
                "Artifact object chunk already contains different bytes",
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            ArtifactError::adapter("Artifact chunk path has no controlled parent")
        })?;
        fs::create_dir_all(parent).map_err(io_error)?;
        write_linked_file(&path, bytes, &artifact_id.0)
    }

    fn finalize(
        &mut self,
        artifact_id: &ArtifactId,
        last_sequence: u64,
        digest: &Sha256Digest,
        size_bytes: u64,
    ) -> Result<(), ArtifactError> {
        let target = self.object_path(digest)?;
        if target.exists() {
            let bytes = fs::read(&target).map_err(io_error)?;
            validate_complete_bytes(digest, size_bytes, &bytes)?;
            remove_upload_directory(&self.root, &self.upload_directory(artifact_id)?)?;
            return Ok(());
        }
        let parent = target.parent().ok_or_else(|| {
            ArtifactError::adapter("Artifact object path has no controlled parent")
        })?;
        fs::create_dir_all(parent).map_err(io_error)?;
        let (temporary, mut output) = temporary_object_file(parent, &artifact_id.0)?;
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        for sequence in 1..=last_sequence {
            let path = self.chunk_path(artifact_id, sequence)?;
            let mut input = File::open(&path).map_err(|_| {
                ArtifactError::new(
                    ArtifactErrorKind::Incomplete,
                    format!("Artifact object chunk {sequence} is missing"),
                )
            })?;
            let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
            loop {
                let read = input.read(&mut buffer).map_err(io_error)?;
                if read == 0 {
                    break;
                }
                output.write_all(&buffer[..read]).map_err(io_error)?;
                hasher.update(&buffer[..read]);
                total = total
                    .checked_add(read as u64)
                    .ok_or_else(|| ArtifactError::invalid("Artifact object size overflow"))?;
            }
        }
        output.sync_all().map_err(io_error)?;
        drop(output);
        let expected = sha256_hex(digest)?;
        let actual = format!("{:x}", hasher.finalize());
        if total != size_bytes || actual != expected {
            let _ = fs::remove_file(&temporary);
            return Err(ArtifactError::digest_mismatch(
                "Artifact object chunks do not match the declared content address",
            ));
        }
        match fs::hard_link(&temporary, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = fs::read(&target).map_err(io_error)?;
                validate_complete_bytes(digest, size_bytes, &existing)?;
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(io_error(error));
            }
        }
        fs::remove_file(&temporary).map_err(io_error)?;
        sync_directory(parent)?;
        remove_upload_directory(&self.root, &self.upload_directory(artifact_id)?)?;
        Ok(())
    }

    fn read(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, ArtifactError> {
        let path = self.object_path(digest)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error(error)),
        }
    }

    fn delete(&mut self, digest: &Sha256Digest) -> Result<(), ArtifactError> {
        let path = self.object_path(digest)?;
        let parent = path.parent().ok_or_else(|| {
            ArtifactError::adapter("Artifact object path has no controlled parent")
        })?;
        match fs::remove_file(&path) {
            Ok(()) => sync_directory(parent),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }
}

/// In-memory object adapter used to prove the same contract expected from an
/// enterprise object-storage implementation.
#[derive(Clone, Default)]
pub struct FakeArtifactObjectStore {
    state: Arc<Mutex<FakeObjectState>>,
}

#[derive(Default)]
struct FakeObjectState {
    chunks: HashMap<(String, u64), Vec<u8>>,
    objects: HashMap<String, Vec<u8>>,
}

impl FakeArtifactObjectStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Test probe that replaces one content-addressed object after acceptance.
    /// Normal Artifact writes cannot mutate a completed object.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed digest or poisoned test lock.
    pub fn corrupt_object(
        &self,
        digest: &Sha256Digest,
        bytes: Vec<u8>,
    ) -> Result<(), ArtifactError> {
        let key = sha256_hex(digest)?.to_owned();
        self.state
            .lock()
            .map_err(|_| ArtifactError::adapter("fake object store lock is poisoned"))?
            .objects
            .insert(key, bytes);
        Ok(())
    }

    /// Returns the number of staged chunks retained by the fake adapter.
    /// This test probe verifies that completion releases upload-only bytes.
    ///
    /// # Errors
    ///
    /// Returns an adapter error if the fake state lock is poisoned.
    pub fn pending_chunk_count(&self) -> Result<usize, ArtifactError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| ArtifactError::adapter("fake object store lock is poisoned"))?
            .chunks
            .len())
    }
}

impl ArtifactObjectStore for FakeArtifactObjectStore {
    fn put_chunk(
        &mut self,
        artifact_id: &ArtifactId,
        sequence: u64,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ArtifactError> {
        canonical_id(&artifact_id.0, "art_", "artifactId")?;
        let expected = sha256_hex(digest)?;
        if format!("{:x}", Sha256::digest(bytes)) != expected {
            return Err(ArtifactError::digest_mismatch(
                "fake object adapter rejected a corrupt chunk",
            ));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| ArtifactError::adapter("fake object store lock is poisoned"))?;
        match state.chunks.get(&(artifact_id.0.clone(), sequence)) {
            Some(existing) if existing == bytes => Ok(()),
            Some(_) => Err(ArtifactError::conflict(
                "fake object chunk already contains different bytes",
            )),
            None => {
                state
                    .chunks
                    .insert((artifact_id.0.clone(), sequence), bytes.to_vec());
                Ok(())
            }
        }
    }

    fn finalize(
        &mut self,
        artifact_id: &ArtifactId,
        last_sequence: u64,
        digest: &Sha256Digest,
        size_bytes: u64,
    ) -> Result<(), ArtifactError> {
        let key = sha256_hex(digest)?.to_owned();
        let mut state = self
            .state
            .lock()
            .map_err(|_| ArtifactError::adapter("fake object store lock is poisoned"))?;
        if let Some(existing) = state.objects.get(&key) {
            validate_complete_bytes(digest, size_bytes, existing)?;
            state
                .chunks
                .retain(|(stored_id, _), _| stored_id != &artifact_id.0);
            return Ok(());
        }
        let mut bytes = Vec::new();
        for sequence in 1..=last_sequence {
            let chunk = state
                .chunks
                .get(&(artifact_id.0.clone(), sequence))
                .ok_or_else(|| {
                    ArtifactError::new(
                        ArtifactErrorKind::Incomplete,
                        format!("fake object chunk {sequence} is missing"),
                    )
                })?;
            bytes.extend_from_slice(chunk);
        }
        validate_incoming_complete_bytes(digest, size_bytes, &bytes)?;
        state.objects.insert(key, bytes);
        state
            .chunks
            .retain(|(stored_id, _), _| stored_id != &artifact_id.0);
        Ok(())
    }

    fn read(&self, digest: &Sha256Digest) -> Result<Option<Vec<u8>>, ArtifactError> {
        let key = sha256_hex(digest)?.to_owned();
        Ok(self
            .state
            .lock()
            .map_err(|_| ArtifactError::adapter("fake object store lock is poisoned"))?
            .objects
            .get(&key)
            .cloned())
    }

    fn delete(&mut self, digest: &Sha256Digest) -> Result<(), ArtifactError> {
        let key = sha256_hex(digest)?.to_owned();
        self.state
            .lock()
            .map_err(|_| ArtifactError::adapter("fake object store lock is poisoned"))?
            .objects
            .remove(&key);
        Ok(())
    }
}

fn migrate_catalog(connection: &mut Connection) -> Result<(), ArtifactError> {
    let version = connection
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    if !matches!(version, 0 | 1 | CATALOG_SCHEMA_VERSION) {
        return Err(ArtifactError::adapter(format!(
            "unsupported Artifact catalog schema version {version}"
        )));
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql_error)?;
    if version == 1 {
        let has_legacy_rows = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM artifacts LIMIT 1)
                     OR EXISTS(SELECT 1 FROM artifact_chunks LIMIT 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(sql_error)?;
        if has_legacy_rows {
            return Err(ArtifactError::adapter(
                "Artifact catalog v1 contains rows without verified metering attribution",
            ));
        }
        transaction
            .execute_batch("DROP TABLE artifact_chunks; DROP TABLE artifacts;")
            .map_err(sql_error)?;
    }
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS artifacts (
                 artifact_id TEXT PRIMARY KEY NOT NULL,
                 scope_key BLOB NOT NULL,
                 open_message_id TEXT UNIQUE NOT NULL,
                 open_request_id TEXT UNIQUE NOT NULL,
                 kind TEXT NOT NULL,
                 media_type TEXT NOT NULL,
                 digest TEXT NOT NULL,
                 size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
                 file_name TEXT,
                 execution_job_id TEXT NOT NULL,
                 attempt INTEGER NOT NULL CHECK (attempt > 0),
                 lease_id TEXT NOT NULL,
                 fencing_token TEXT NOT NULL,
                 worker_id TEXT NOT NULL,
                 worker_instance_id TEXT NOT NULL,
                 worker_session_id TEXT NOT NULL,
                 retention_kind TEXT NOT NULL CHECK (retention_kind IN ('until', 'indefinite')),
                 retention_until_millis INTEGER,
                 created_at_millis INTEGER NOT NULL CHECK (created_at_millis > 0),
                 acknowledged_sequence INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged_sequence >= 0),
                 complete INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0, 1)),
                 deleted_at_millis INTEGER CHECK (deleted_at_millis > 0),
                 metering_attribution_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS artifacts_digest ON artifacts (digest);
             CREATE TABLE IF NOT EXISTS artifact_chunks (
                 artifact_id TEXT NOT NULL,
                 message_id TEXT UNIQUE NOT NULL,
                 sent_at_millis INTEGER NOT NULL CHECK (sent_at_millis > 0),
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 content_type TEXT NOT NULL,
                 digest TEXT NOT NULL,
                 size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
                 is_final INTEGER NOT NULL CHECK (is_final IN (0, 1)),
                 PRIMARY KEY (artifact_id, sequence),
                 FOREIGN KEY (artifact_id) REFERENCES artifacts (artifact_id) ON DELETE CASCADE
             );",
        )
        .map_err(sql_error)?;
    transaction
        .execute_batch(metering::SCHEMA)
        .map_err(sql_error)?;
    validate_catalog_schema(&transaction)?;
    transaction
        .pragma_update(None, "user_version", CATALOG_SCHEMA_VERSION)
        .map_err(sql_error)?;
    transaction.commit().map_err(sql_error)
}

fn validate_catalog_schema(transaction: &rusqlite::Transaction<'_>) -> Result<(), ArtifactError> {
    let artifact_columns = table_columns(transaction, "artifacts")?;
    if artifact_columns
        != [
            "artifact_id",
            "scope_key",
            "open_message_id",
            "open_request_id",
            "kind",
            "media_type",
            "digest",
            "size_bytes",
            "file_name",
            "execution_job_id",
            "attempt",
            "lease_id",
            "fencing_token",
            "worker_id",
            "worker_instance_id",
            "worker_session_id",
            "retention_kind",
            "retention_until_millis",
            "created_at_millis",
            "acknowledged_sequence",
            "complete",
            "deleted_at_millis",
            "metering_attribution_json",
        ]
    {
        return Err(ArtifactError::adapter(
            "Artifact metadata schema is not canonical",
        ));
    }
    validate_text_not_null_column(transaction, "artifacts", "metering_attribution_json")?;
    let chunk_columns = table_columns(transaction, "artifact_chunks")?;
    if chunk_columns
        != [
            "artifact_id",
            "message_id",
            "sent_at_millis",
            "sequence",
            "content_type",
            "digest",
            "size_bytes",
            "is_final",
        ]
    {
        return Err(ArtifactError::adapter(
            "Artifact chunk metadata schema is not canonical",
        ));
    }
    metering::validate_schema(transaction)?;
    Ok(())
}

fn validate_text_not_null_column(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    column: &str,
) -> Result<(), ArtifactError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = transaction.prepare(&pragma).map_err(sql_error)?;
    let mut rows = statement.query([]).map_err(sql_error)?;
    let mut actual = None;
    while actual.is_none() {
        let Some(row) = rows.next().map_err(sql_error)? else {
            break;
        };
        if row.get::<_, String>(1).map_err(sql_error)? == column {
            actual = Some((
                row.get::<_, String>(2).map_err(sql_error)?,
                row.get::<_, bool>(3).map_err(sql_error)?,
            ));
        }
    }
    if actual
        .as_ref()
        .is_some_and(|(kind, not_null)| kind.eq_ignore_ascii_case("TEXT") && *not_null)
    {
        Ok(())
    } else {
        Err(ArtifactError::adapter(
            "Artifact metering attribution column is not canonical",
        ))
    }
}

fn table_columns(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
) -> Result<Vec<String>, ArtifactError> {
    let pragma = match table {
        "artifacts" => "PRAGMA table_info(artifacts)",
        "artifact_chunks" => "PRAGMA table_info(artifact_chunks)",
        _ => return Err(ArtifactError::adapter("unknown Artifact catalog table")),
    };
    let mut statement = transaction.prepare(pragma).map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(sql_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
}

fn insert_open(
    transaction: &rusqlite::Transaction<'_>,
    open: &ArtifactOpen,
) -> Result<(), ArtifactError> {
    let (retention_kind, retention_until) = match open.retention {
        ArtifactRetention::UntilMillis(value) => (
            "until",
            Some(i64_value(value, "Artifact retention timestamp")?),
        ),
        ArtifactRetention::Indefinite => ("indefinite", None),
    };
    transaction
        .execute(
            "INSERT INTO artifacts
             (artifact_id, scope_key, open_message_id, open_request_id, kind, media_type,
              digest, size_bytes, file_name, execution_job_id, attempt, lease_id, fencing_token, worker_id,
              worker_instance_id, worker_session_id, retention_kind,
              retention_until_millis, created_at_millis, acknowledged_sequence, complete,
              metering_attribution_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18, ?19, 0, 0, ?20)",
            params![
                open.artifact_id.0,
                open.scope_key.as_bytes(),
                open.message_id.0,
                open.request_id.0,
                open.kind,
                open.media_type,
                open.digest.0,
                i64_value(open.size_bytes, "Artifact size")?,
                open.file_name,
                open.provenance.execution_job_id.0,
                i64_value(open.provenance.attempt, "Artifact provenance attempt")?,
                open.provenance.lease_id.0,
                open.provenance.fencing_token.0,
                open.provenance.worker_id.0,
                open.provenance.worker_instance_id.0,
                open.provenance.worker_session_id.0,
                retention_kind,
                retention_until,
                i64_value(open.created_at_millis, "Artifact creation timestamp")?,
                serde_json::to_string(&open.metering_attribution).map_err(|_| {
                    ArtifactError::invalid("Artifact metering attribution is not serializable")
                })?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn load_record(
    connection: &Connection,
    artifact_id: &ArtifactId,
) -> Result<Option<ArtifactRecord>, ArtifactError> {
    let record = connection
        .query_row(
            "SELECT scope_key, open_message_id, open_request_id, kind, media_type, digest,
                    size_bytes, file_name, execution_job_id, attempt, lease_id, fencing_token, worker_id,
                    worker_instance_id, worker_session_id, retention_kind,
                    retention_until_millis, created_at_millis, acknowledged_sequence, complete,
                    deleted_at_millis, metering_attribution_json
             FROM artifacts WHERE artifact_id = ?1",
            [&artifact_id.0],
            |row| artifact_record_row(row, artifact_id),
        )
        .optional()
        .map_err(sql_error)?;
    if let Some(record) = &record {
        record.open.validate().map_err(|_| {
            ArtifactError::corrupt("Artifact catalog contains invalid immutable metadata")
        })?;
        if (record.complete && record.acknowledged_sequence == 0)
            || record
                .deleted_at_millis
                .is_some_and(|deleted| deleted < record.open.created_at_millis)
        {
            return Err(ArtifactError::corrupt(
                "Artifact catalog contains inconsistent lifecycle metadata",
            ));
        }
        if record.complete {
            metering::require_complete_source(connection, record)?;
        }
    }
    Ok(record)
}

fn artifact_record_row(
    row: &rusqlite::Row<'_>,
    artifact_id: &ArtifactId,
) -> rusqlite::Result<ArtifactRecord> {
    let retention_kind = row.get::<_, String>(15)?;
    let retention_until = row.get::<_, Option<i64>>(16)?;
    let retention = match (retention_kind.as_str(), retention_until) {
        ("until", Some(value)) if value >= 0 => {
            ArtifactRetention::UntilMillis(value.cast_unsigned())
        }
        ("indefinite", None) => ArtifactRetention::Indefinite,
        _ => return Err(invalid_artifact_column(15, "retention_kind")),
    };
    let attempt = row.get::<_, i64>(9)?;
    let size_bytes = row.get::<_, i64>(6)?;
    let created_at_millis = row.get::<_, i64>(17)?;
    let acknowledged_sequence = row.get::<_, i64>(18)?;
    if attempt <= 0 || size_bytes < 0 || created_at_millis <= 0 || acknowledged_sequence < 0 {
        return Err(rusqlite::Error::IntegralValueOutOfRange(0, attempt));
    }
    let attribution_json = row.get::<_, String>(21)?;
    let metering_attribution: ArtifactMeteringAttribution = serde_json::from_str(&attribution_json)
        .map_err(|_| invalid_artifact_column(21, "metering_attribution_json"))?;
    if serde_json::to_string(&metering_attribution).ok().as_deref() != Some(&attribution_json) {
        return Err(invalid_artifact_column(21, "metering_attribution_json"));
    }
    Ok(ArtifactRecord {
        open: ArtifactOpen {
            scope_key: ReceiptScopeKey::from_encoded(row.get(0)?)
                .map_err(|_| invalid_artifact_column(0, "scope_key"))?,
            message_id: ExecutionMessageId(row.get(1)?),
            request_id: RequestId(row.get(2)?),
            artifact_id: artifact_id.clone(),
            kind: row.get(3)?,
            media_type: row.get(4)?,
            digest: Sha256Digest(row.get(5)?),
            size_bytes: size_bytes.cast_unsigned(),
            file_name: row.get(7)?,
            provenance: ArtifactProvenance {
                execution_job_id: ExecutionJobId(row.get(8)?),
                attempt: attempt.cast_unsigned(),
                lease_id: LeaseId(row.get(10)?),
                fencing_token: FencingToken(row.get(11)?),
                worker_id: WorkerId(row.get(12)?),
                worker_instance_id: WorkerInstanceId(row.get(13)?),
                worker_session_id: WorkerSessionId(row.get(14)?),
            },
            metering_attribution,
            retention,
            created_at_millis: created_at_millis.cast_unsigned(),
        },
        acknowledged_sequence: acknowledged_sequence.cast_unsigned(),
        complete: row.get::<_, i64>(19)? == 1,
        deleted_at_millis: row
            .get::<_, Option<i64>>(20)?
            .map(|value| {
                u64::try_from(value)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(20, value))
            })
            .transpose()?,
    })
}

fn invalid_artifact_column(index: usize, name: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(index, name.into(), rusqlite::types::Type::Text)
}

fn load_record_by_open_identity(
    connection: &Connection,
    message_id: &ExecutionMessageId,
    request_id: &RequestId,
) -> Result<Option<ArtifactRecord>, ArtifactError> {
    let mut statement = connection
        .prepare(
            "SELECT artifact_id FROM artifacts
             WHERE open_message_id = ?1 OR open_request_id = ?2
             ORDER BY artifact_id",
        )
        .map_err(sql_error)?;
    let artifact_ids = statement
        .query_map(params![message_id.0, request_id.0], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    match artifact_ids.as_slice() {
        [] => Ok(None),
        [artifact_id] => load_record(connection, &ArtifactId(artifact_id.clone())),
        _ => Err(ArtifactError::conflict(
            "Artifact open message and request identities resolve to different records",
        )),
    }
}

fn unfinished_quota_opens_for_job(
    connection: &Connection,
    execution_job_id: &ExecutionJobId,
) -> Result<Vec<ArtifactOpen>, ArtifactError> {
    let mut statement = connection
        .prepare(
            "SELECT artifact_id FROM artifacts
             WHERE execution_job_id = ?1 AND complete = 0 AND deleted_at_millis IS NULL
             ORDER BY artifact_id LIMIT ?2",
        )
        .map_err(sql_error)?;
    let limit = i64::try_from(MAX_UNFINISHED_ARTIFACTS_PER_JOB + 1)
        .expect("unfinished Artifact bound fits SQLite");
    let artifact_ids = statement
        .query_map(params![execution_job_id.0, limit], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    if artifact_ids.len() > MAX_UNFINISHED_ARTIFACTS_PER_JOB {
        return Err(ArtifactError::adapter(
            "unfinished Artifact quota authority exceeds the supported bound",
        ));
    }
    artifact_ids
        .into_iter()
        .map(|artifact_id| {
            let record = load_record(connection, &ArtifactId(artifact_id))?.ok_or_else(|| {
                ArtifactError::corrupt("unfinished Artifact disappeared from its catalog read")
            })?;
            if record.complete
                || record.deleted_at_millis.is_some()
                || record.open.provenance.execution_job_id != *execution_job_id
            {
                return Err(ArtifactError::corrupt(
                    "unfinished Artifact quota authority differs from its catalog index",
                ));
            }
            Ok(record.open)
        })
        .collect()
}

fn replay_open_identity(
    connection: &Connection,
    open: &ArtifactOpen,
) -> Result<Option<ArtifactRecord>, ArtifactError> {
    if let Some(existing) =
        load_record_by_open_identity(connection, &open.message_id, &open.request_id)?
    {
        if existing.open.scope_key != open.scope_key {
            return Err(ArtifactError::new(
                ArtifactErrorKind::PermissionDenied,
                "Artifact message identity belongs to another repository scope",
            ));
        }
        if existing.open != *open {
            return Err(ArtifactError::conflict(
                "Artifact open message identity was reused with different metadata",
            ));
        }
        return Ok(Some(existing));
    }
    Ok(None)
}

fn validate_complete_bytes(
    digest: &Sha256Digest,
    size_bytes: u64,
    bytes: &[u8],
) -> Result<(), ArtifactError> {
    let expected = sha256_hex(digest)?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if bytes.len() as u64 != size_bytes || actual != expected {
        return Err(ArtifactError::corrupt(
            "Artifact object bytes do not match immutable metadata",
        ));
    }
    Ok(())
}

fn validate_incoming_complete_bytes(
    digest: &Sha256Digest,
    size_bytes: u64,
    bytes: &[u8],
) -> Result<(), ArtifactError> {
    let expected = sha256_hex(digest)?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if bytes.len() as u64 != size_bytes || actual != expected {
        return Err(ArtifactError::digest_mismatch(
            "Artifact chunks do not match immutable open metadata",
        ));
    }
    Ok(())
}

fn canonical_id(value: &str, prefix: &str, field: &str) -> Result<(), ArtifactError> {
    let suffix = value
        .strip_prefix(prefix)
        .ok_or_else(|| ArtifactError::invalid(format!("Artifact {field} has the wrong prefix")))?;
    if suffix.len() != 26 || !suffix.bytes().all(crockford_byte) {
        return Err(ArtifactError::invalid(format!(
            "Artifact {field} is not canonical"
        )));
    }
    Ok(())
}

fn crockford_byte(byte: u8) -> bool {
    byte.is_ascii_digit()
        || matches!(
            byte,
            b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z'
        )
}

fn sha256_hex(digest: &Sha256Digest) -> Result<&str, ArtifactError> {
    let value = digest
        .0
        .strip_prefix("sha256:")
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
        .ok_or_else(|| ArtifactError::invalid("Artifact digest is not canonical SHA-256"))?;
    Ok(value)
}

fn bounded_text(value: &str, field: &str, maximum: usize) -> Result<(), ArtifactError> {
    if value.is_empty()
        || value.len() > maximum.min(MAX_TEXT_BYTES)
        || value.bytes().any(|byte| byte <= 31 || byte == 127)
    {
        return Err(ArtifactError::invalid(format!("{field} is invalid")));
    }
    Ok(())
}

fn i64_value(value: u64, field: &str) -> Result<i64, ArtifactError> {
    value
        .try_into()
        .map_err(|_| ArtifactError::invalid(format!("{field} exceeds the SQLite range")))
}

fn write_linked_file(path: &Path, bytes: &[u8], label: &str) -> Result<(), ArtifactError> {
    let parent = path
        .parent()
        .ok_or_else(|| ArtifactError::adapter("Artifact file has no controlled parent"))?;
    let (temporary, mut file) = temporary_object_file(parent, label)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)?;
    drop(file);
    match fs::hard_link(&temporary, path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(path).map_err(io_error)?;
            if existing != bytes {
                let _ = fs::remove_file(&temporary);
                return Err(ArtifactError::conflict(
                    "Artifact object chunk already contains different bytes",
                ));
            }
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(io_error(error));
        }
    }
    fs::remove_file(&temporary).map_err(io_error)?;
    sync_directory(parent)
}

fn temporary_object_file(parent: &Path, label: &str) -> Result<(PathBuf, File), ArtifactError> {
    for _ in 0..1_024 {
        let nonce = NEXT_TEMPORARY_OBJECT_FILE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".{label}.{}.{}.tmp", std::process::id(), nonce));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    Err(ArtifactError::adapter(
        "Artifact object adapter could not allocate a temporary file",
    ))
}

fn sync_directory(path: &Path) -> Result<(), ArtifactError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
}

fn remove_upload_directory(root: &Path, upload_directory: &Path) -> Result<(), ArtifactError> {
    match fs::remove_dir_all(upload_directory) {
        Ok(()) => sync_directory(&root.join("uploads")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(error)),
    }
}

fn sql_error(error: rusqlite::Error) -> ArtifactError {
    let message = format!("Artifact catalog operation failed: {error}");
    drop(error);
    ArtifactError::adapter(message)
}

fn io_error(error: std::io::Error) -> ArtifactError {
    let message = format!("Artifact object operation failed: {error}");
    drop(error);
    ArtifactError::adapter(message)
}
