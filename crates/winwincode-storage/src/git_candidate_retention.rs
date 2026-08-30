// SPDX-License-Identifier: Apache-2.0

//! Durable Git references for acknowledged candidate Artifacts.
//!
//! The canonical Control Plane `SQLite` connection owns every intent and
//! receipt. Git is a non-transactional side effect, so pin and release use a
//! durable two-phase state machine and exact compare-and-swap reference
//! updates. Opening this seam reconciles every unfinished side effect before
//! accepting another operation.

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rusqlite::{Connection, OptionalExtension as _, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{ArtifactId, DeliveryId, Sha256Digest};

use crate::{
    ArtifactWriteReceipt, SqliteStorage, StorageError, ValidatedGitSourceArtifact, sql_error,
};

const RECORD_SCHEMA_VERSION: u8 = 1;
const LOCK_FILE_NAME: &str = "git-candidate-retention.lock";
const REFERENCE_PREFIX: &str = "refs/winwincode/candidates/";
const MAX_LOCATOR_BYTES: usize = 4_096;
const MAX_ID_BYTES: usize = 512;
const ZERO_SHA1: &str = "0000000000000000000000000000000000000000";
const ZERO_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS git_candidate_retentions (
    binding_key TEXT PRIMARY KEY NOT NULL,
    artifact_id TEXT NOT NULL UNIQUE,
    reference_name TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (
        state IN ('pin_intent', 'pinned', 'release_intent', 'released')
    ),
    record_json BLOB NOT NULL
);
";

/// Stable failure categories for candidate Git retention.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateGitRetentionErrorKind {
    InvalidInput,
    NotFound,
    Conflict,
    PermissionDenied,
    Corrupt,
    Adapter,
    Closed,
}

/// Secret-safe candidate Git retention failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateGitRetentionError {
    kind: CandidateGitRetentionErrorKind,
    message: String,
}

impl CandidateGitRetentionError {
    fn new(kind: CandidateGitRetentionErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(CandidateGitRetentionErrorKind::InvalidInput, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(CandidateGitRetentionErrorKind::Conflict, message)
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::new(CandidateGitRetentionErrorKind::Corrupt, message)
    }

    fn adapter(message: impl Into<String>) -> Self {
        Self::new(CandidateGitRetentionErrorKind::Adapter, message)
    }

    #[must_use]
    pub const fn kind(&self) -> CandidateGitRetentionErrorKind {
        self.kind
    }
}

impl fmt::Display for CandidateGitRetentionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CandidateGitRetentionError {}

/// Durable state of one candidate reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateGitRetentionState {
    PinIntent,
    Pinned,
    ReleaseIntent,
    Released,
}

impl CandidateGitRetentionState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PinIntent => "pin_intent",
            Self::Pinned => "pinned",
            Self::ReleaseIntent => "release_intent",
            Self::Released => "released",
        }
    }

    fn parse(value: &str) -> Result<Self, CandidateGitRetentionError> {
        match value {
            "pin_intent" => Ok(Self::PinIntent),
            "pinned" => Ok(Self::Pinned),
            "release_intent" => Ok(Self::ReleaseIntent),
            "released" => Ok(Self::Released),
            _ => Err(CandidateGitRetentionError::corrupt(
                "candidate Git retention state is invalid",
            )),
        }
    }
}

/// Delivery terminal that permits reference release after reads are closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateGitTerminalOutcome {
    Delivered,
    Rejected,
    Cancelled,
}

/// Exact terminal Delivery and read-closure facts authorizing release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateGitReleaseAuthority {
    delivery_id: DeliveryId,
    terminal_outcome: CandidateGitTerminalOutcome,
    terminal_receipt_digest: Sha256Digest,
    reads_closed_receipt_digest: Sha256Digest,
}

impl CandidateGitReleaseAuthority {
    /// Builds the explicit Delivery terminal/read-closure authority.
    ///
    /// The application layer must issue `reads_closed_receipt_digest` only
    /// after every verification and rework reader has reached a durable final
    /// state.
    ///
    /// # Errors
    ///
    /// Rejects malformed Delivery or receipt identities.
    pub fn delivery_final_without_future_reads(
        delivery_id: DeliveryId,
        terminal_outcome: CandidateGitTerminalOutcome,
        terminal_receipt_digest: Sha256Digest,
        reads_closed_receipt_digest: Sha256Digest,
    ) -> Result<Self, CandidateGitRetentionError> {
        canonical_public_id(&delivery_id.0, "dlv_", "deliveryId")?;
        sha256_digest(&terminal_receipt_digest.0, "terminalReceiptDigest")?;
        sha256_digest(&reads_closed_receipt_digest.0, "readsClosedReceiptDigest")?;
        if terminal_receipt_digest == reads_closed_receipt_digest {
            return Err(CandidateGitRetentionError::invalid(
                "terminal and read-closure receipts must be distinct",
            ));
        }
        Ok(Self {
            delivery_id,
            terminal_outcome,
            terminal_receipt_digest,
            reads_closed_receipt_digest,
        })
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn terminal_outcome(&self) -> CandidateGitTerminalOutcome {
        self.terminal_outcome
    }

    /// Returns the durable terminal-outcome receipt digest authorizing release.
    #[must_use]
    pub const fn terminal_receipt_digest(&self) -> &Sha256Digest {
        &self.terminal_receipt_digest
    }

    /// Returns the durable read-closure receipt digest authorizing release.
    #[must_use]
    pub const fn reads_closed_receipt_digest(&self) -> &Sha256Digest {
        &self.reads_closed_receipt_digest
    }
}

/// Stable result of pinning one acknowledged candidate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateGitPinReceipt {
    artifact_id: ArtifactId,
    artifact_digest: Sha256Digest,
    delivery_id: DeliveryId,
    repository_locator: String,
    candidate_commit_id: String,
    candidate_tree_id: String,
    reference_name: String,
    receipt_digest: Sha256Digest,
    state: CandidateGitRetentionState,
    idempotent_replay: bool,
}

impl CandidateGitPinReceipt {
    #[must_use]
    pub const fn artifact_id(&self) -> &ArtifactId {
        &self.artifact_id
    }

    #[must_use]
    pub const fn artifact_digest(&self) -> &Sha256Digest {
        &self.artifact_digest
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub fn repository_locator(&self) -> &str {
        &self.repository_locator
    }

    #[must_use]
    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    #[must_use]
    pub fn candidate_tree_id(&self) -> &str {
        &self.candidate_tree_id
    }

    #[must_use]
    pub fn reference_name(&self) -> &str {
        &self.reference_name
    }

    #[must_use]
    pub const fn receipt_digest(&self) -> &Sha256Digest {
        &self.receipt_digest
    }

    #[must_use]
    pub const fn state(&self) -> CandidateGitRetentionState {
        self.state
    }

    #[must_use]
    pub const fn is_idempotent_replay(&self) -> bool {
        self.idempotent_replay
    }
}

/// Stable receipt committed before the Git reference is deleted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateGitReleaseReceipt {
    artifact_id: ArtifactId,
    reference_name: String,
    delivery_id: DeliveryId,
    terminal_outcome: CandidateGitTerminalOutcome,
    receipt_digest: Sha256Digest,
    state: CandidateGitRetentionState,
    idempotent_replay: bool,
}

impl CandidateGitReleaseReceipt {
    #[must_use]
    pub const fn artifact_id(&self) -> &ArtifactId {
        &self.artifact_id
    }

    #[must_use]
    pub fn reference_name(&self) -> &str {
        &self.reference_name
    }

    #[must_use]
    pub const fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn terminal_outcome(&self) -> CandidateGitTerminalOutcome {
        self.terminal_outcome
    }

    #[must_use]
    pub const fn receipt_digest(&self) -> &Sha256Digest {
        &self.receipt_digest
    }

    #[must_use]
    pub const fn state(&self) -> CandidateGitRetentionState {
        self.state
    }

    #[must_use]
    pub const fn is_idempotent_replay(&self) -> bool {
        self.idempotent_replay
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRelease {
    delivery_id: String,
    terminal_outcome: CandidateGitTerminalOutcome,
    terminal_receipt_digest: String,
    reads_closed_receipt_digest: String,
    release_receipt_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRetention {
    schema_version: u8,
    binding_key: String,
    artifact_id: String,
    artifact_digest: String,
    delivery_id: String,
    repository_locator: String,
    repository_path: String,
    git_common_directory: String,
    candidate_commit_id: String,
    candidate_tree_id: String,
    final_ack_digest: String,
    reference_name: String,
    pin_receipt_digest: String,
    state: CandidateGitRetentionState,
    release: Option<StoredRelease>,
}

impl StoredRetention {
    fn from_final_ack(
        acknowledgement: &ArtifactWriteReceipt,
        source: &ValidatedGitSourceArtifact,
        final_ack_digest: &Sha256Digest,
        repository: &RepositoryIdentity,
    ) -> Result<Self, CandidateGitRetentionError> {
        if !acknowledgement.is_complete()
            || acknowledgement.record() != source.artifact()
            || source.artifact().deleted_at_millis().is_some()
            || source.artifact().kind() != "candidate"
        {
            return Err(CandidateGitRetentionError::conflict(
                "pin requires the exact complete candidate Artifact acknowledgement",
            ));
        }
        sha256_digest(&final_ack_digest.0, "finalArtifactAckDigest")?;
        canonical_public_id(&source.artifact().artifact_id().0, "art_", "artifactId")?;
        sha256_digest(&source.artifact().digest().0, "artifactDigest")?;
        let delivery_id = source
            .artifact()
            .metering_attribution()
            .delivery_id
            .as_ref()
            .ok_or_else(|| {
                CandidateGitRetentionError::conflict(
                    "candidate Artifact is not attributed to a Delivery",
                )
            })?;
        canonical_public_id(&delivery_id.0, "dlv_", "deliveryId")?;
        portable_locator(source.repository_locator())?;
        git_object_id(source.candidate_commit_id(), "candidateCommitId")?;
        git_object_id(source.candidate_tree_id(), "candidateTreeId")?;
        let repository_path = path_text(&repository.repository, "repository path")?;
        let git_common_directory = path_text(&repository.git_common_directory, "Git directory")?;
        let binding_key = binding_key(
            &source.artifact().artifact_id().0,
            &source.artifact().digest().0,
            source.repository_locator(),
            &git_common_directory,
            source.candidate_commit_id(),
            source.candidate_tree_id(),
        );
        let reference_name = format!("{REFERENCE_PREFIX}{binding_key}");
        let pin_receipt_digest =
            pin_receipt_digest(&binding_key, &reference_name, &final_ack_digest.0);
        let record = Self {
            schema_version: RECORD_SCHEMA_VERSION,
            binding_key,
            artifact_id: source.artifact().artifact_id().0.clone(),
            artifact_digest: source.artifact().digest().0.clone(),
            delivery_id: delivery_id.0.clone(),
            repository_locator: source.repository_locator().to_owned(),
            repository_path,
            git_common_directory,
            candidate_commit_id: source.candidate_commit_id().to_owned(),
            candidate_tree_id: source.candidate_tree_id().to_owned(),
            final_ack_digest: final_ack_digest.0.clone(),
            reference_name,
            pin_receipt_digest,
            state: CandidateGitRetentionState::PinIntent,
            release: None,
        };
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), CandidateGitRetentionError> {
        if self.schema_version != RECORD_SCHEMA_VERSION {
            return Err(CandidateGitRetentionError::corrupt(
                "candidate Git retention schema version is unsupported",
            ));
        }
        canonical_public_id(&self.artifact_id, "art_", "artifactId")?;
        sha256_digest(&self.artifact_digest, "artifactDigest")?;
        canonical_public_id(&self.delivery_id, "dlv_", "deliveryId")?;
        portable_locator(&self.repository_locator)?;
        absolute_path(&self.repository_path, "repository path")?;
        absolute_path(&self.git_common_directory, "Git directory")?;
        git_object_id(&self.candidate_commit_id, "candidateCommitId")?;
        git_object_id(&self.candidate_tree_id, "candidateTreeId")?;
        sha256_digest(&self.final_ack_digest, "finalArtifactAckDigest")?;
        sha256_hex(&self.binding_key, "bindingKey")?;
        sha256_digest(&self.pin_receipt_digest, "pinReceiptDigest")?;
        if self.reference_name != format!("{REFERENCE_PREFIX}{}", self.binding_key)
            || self.binding_key
                != binding_key(
                    &self.artifact_id,
                    &self.artifact_digest,
                    &self.repository_locator,
                    &self.git_common_directory,
                    &self.candidate_commit_id,
                    &self.candidate_tree_id,
                )
            || self.pin_receipt_digest
                != pin_receipt_digest(
                    &self.binding_key,
                    &self.reference_name,
                    &self.final_ack_digest,
                )
        {
            return Err(CandidateGitRetentionError::corrupt(
                "candidate Git retention binding is inconsistent",
            ));
        }
        match (self.state, &self.release) {
            (CandidateGitRetentionState::PinIntent | CandidateGitRetentionState::Pinned, None) => {}
            (
                CandidateGitRetentionState::ReleaseIntent | CandidateGitRetentionState::Released,
                Some(release),
            ) => release.validate(self)?,
            _ => {
                return Err(CandidateGitRetentionError::corrupt(
                    "candidate Git retention release state is incomplete",
                ));
            }
        }
        Ok(())
    }

    fn with_release(
        mut self,
        authority: &CandidateGitReleaseAuthority,
    ) -> Result<Self, CandidateGitRetentionError> {
        let release_receipt_digest = release_receipt_digest(
            &self.binding_key,
            &self.pin_receipt_digest,
            &authority.delivery_id.0,
            authority.terminal_outcome,
            &authority.terminal_receipt_digest.0,
            &authority.reads_closed_receipt_digest.0,
        );
        self.state = CandidateGitRetentionState::ReleaseIntent;
        self.release = Some(StoredRelease {
            delivery_id: authority.delivery_id.0.clone(),
            terminal_outcome: authority.terminal_outcome,
            terminal_receipt_digest: authority.terminal_receipt_digest.0.clone(),
            reads_closed_receipt_digest: authority.reads_closed_receipt_digest.0.clone(),
            release_receipt_digest,
        });
        self.validate()?;
        Ok(self)
    }

    fn pin_receipt(&self, idempotent_replay: bool) -> CandidateGitPinReceipt {
        CandidateGitPinReceipt {
            artifact_id: ArtifactId(self.artifact_id.clone()),
            artifact_digest: Sha256Digest(self.artifact_digest.clone()),
            delivery_id: DeliveryId(self.delivery_id.clone()),
            repository_locator: self.repository_locator.clone(),
            candidate_commit_id: self.candidate_commit_id.clone(),
            candidate_tree_id: self.candidate_tree_id.clone(),
            reference_name: self.reference_name.clone(),
            receipt_digest: Sha256Digest(self.pin_receipt_digest.clone()),
            state: self.state,
            idempotent_replay,
        }
    }

    fn release_receipt(
        &self,
        idempotent_replay: bool,
    ) -> Result<CandidateGitReleaseReceipt, CandidateGitRetentionError> {
        let release = self.release.as_ref().ok_or_else(|| {
            CandidateGitRetentionError::corrupt("candidate Git release receipt is missing")
        })?;
        Ok(CandidateGitReleaseReceipt {
            artifact_id: ArtifactId(self.artifact_id.clone()),
            reference_name: self.reference_name.clone(),
            delivery_id: DeliveryId(release.delivery_id.clone()),
            terminal_outcome: release.terminal_outcome,
            receipt_digest: Sha256Digest(release.release_receipt_digest.clone()),
            state: self.state,
            idempotent_replay,
        })
    }
}

impl StoredRelease {
    fn validate(&self, retention: &StoredRetention) -> Result<(), CandidateGitRetentionError> {
        canonical_public_id(&self.delivery_id, "dlv_", "deliveryId")?;
        sha256_digest(&self.terminal_receipt_digest, "terminalReceiptDigest")?;
        sha256_digest(
            &self.reads_closed_receipt_digest,
            "readsClosedReceiptDigest",
        )?;
        sha256_digest(&self.release_receipt_digest, "releaseReceiptDigest")?;
        if self.terminal_receipt_digest == self.reads_closed_receipt_digest
            || self.release_receipt_digest
                != release_receipt_digest(
                    &retention.binding_key,
                    &retention.pin_receipt_digest,
                    &self.delivery_id,
                    self.terminal_outcome,
                    &self.terminal_receipt_digest,
                    &self.reads_closed_receipt_digest,
                )
        {
            return Err(CandidateGitRetentionError::corrupt(
                "candidate Git release authority is inconsistent",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RepositoryIdentity {
    repository: PathBuf,
    git_common_directory: PathBuf,
}

trait GitReferenceBackend {
    fn repository_identity(
        &self,
        controlled_root: &Path,
        repository_locator: &str,
    ) -> Result<RepositoryIdentity, CandidateGitRetentionError>;

    fn ensure_candidate_reference(
        &self,
        repository: &RepositoryIdentity,
        record: &StoredRetention,
    ) -> Result<(), CandidateGitRetentionError>;

    fn delete_candidate_reference(
        &self,
        repository: &RepositoryIdentity,
        record: &StoredRetention,
    ) -> Result<(), CandidateGitRetentionError>;
}

struct SystemGit;

impl GitReferenceBackend for SystemGit {
    fn repository_identity(
        &self,
        controlled_root: &Path,
        repository_locator: &str,
    ) -> Result<RepositoryIdentity, CandidateGitRetentionError> {
        portable_locator(repository_locator)?;
        let repository =
            fs::canonicalize(controlled_root.join(repository_locator)).map_err(|_| {
                CandidateGitRetentionError::new(
                    CandidateGitRetentionErrorKind::NotFound,
                    "controlled Git repository is unavailable",
                )
            })?;
        if !repository.starts_with(controlled_root) || !repository.is_dir() {
            return Err(CandidateGitRetentionError::new(
                CandidateGitRetentionErrorKind::PermissionDenied,
                "repository locator escapes the controlled root",
            ));
        }
        let bare = git_text(
            &repository,
            &["rev-parse".into(), "--is-bare-repository".into()],
        )?;
        if bare != "false" {
            return Err(CandidateGitRetentionError::invalid(
                "controlled source must be a non-bare Git repository",
            ));
        }
        let common = git_text(
            &repository,
            &["rev-parse".into(), "--git-common-dir".into()],
        )?;
        let common = Path::new(&common);
        let common = if common.is_absolute() {
            common.to_path_buf()
        } else {
            repository.join(common)
        };
        let git_common_directory = fs::canonicalize(common).map_err(|_| {
            CandidateGitRetentionError::new(
                CandidateGitRetentionErrorKind::NotFound,
                "controlled Git common directory is unavailable",
            )
        })?;
        Ok(RepositoryIdentity {
            repository,
            git_common_directory,
        })
    }

    fn ensure_candidate_reference(
        &self,
        repository: &RepositoryIdentity,
        record: &StoredRetention,
    ) -> Result<(), CandidateGitRetentionError> {
        verify_candidate_objects(repository, record)?;
        match reference_target(&repository.repository, &record.reference_name)? {
            Some(target) if target == record.candidate_commit_id => return Ok(()),
            Some(_) => {
                return Err(CandidateGitRetentionError::conflict(
                    "candidate Git reference points to a foreign object",
                ));
            }
            None => {}
        }
        let zero = zero_object_id(record.candidate_commit_id.len())?;
        let output = git_status(
            &repository.repository,
            &[
                "update-ref".into(),
                "--no-deref".into(),
                record.reference_name.clone().into(),
                record.candidate_commit_id.clone().into(),
                zero.into(),
            ],
        )?;
        if !output.status.success()
            && reference_target(&repository.repository, &record.reference_name)?
                != Some(record.candidate_commit_id.clone())
        {
            return Err(CandidateGitRetentionError::conflict(
                "candidate Git reference create compare-and-swap failed",
            ));
        }
        Ok(())
    }

    fn delete_candidate_reference(
        &self,
        repository: &RepositoryIdentity,
        record: &StoredRetention,
    ) -> Result<(), CandidateGitRetentionError> {
        match reference_target(&repository.repository, &record.reference_name)? {
            None => return Ok(()),
            Some(target) if target == record.candidate_commit_id => {}
            Some(_) => {
                return Err(CandidateGitRetentionError::conflict(
                    "candidate Git reference changed before release",
                ));
            }
        }
        let output = git_status(
            &repository.repository,
            &[
                "update-ref".into(),
                "--no-deref".into(),
                "-d".into(),
                record.reference_name.clone().into(),
                record.candidate_commit_id.clone().into(),
            ],
        )?;
        if !output.status.success()
            && reference_target(&repository.repository, &record.reference_name)?.is_some()
        {
            return Err(CandidateGitRetentionError::conflict(
                "candidate Git reference delete compare-and-swap failed",
            ));
        }
        Ok(())
    }
}

/// Borrowed candidate-retention operations over the canonical Control Plane
/// `SQLite` connection.
pub struct CandidateGitRetention<'storage> {
    connection: &'storage mut Connection,
    controlled_root: PathBuf,
    owner_lock_path: PathBuf,
    git: Box<dyn GitReferenceBackend>,
}

impl SqliteStorage {
    /// Opens the candidate Git retention seam over this storage connection.
    ///
    /// `controlled_repository_root` contains repositories only. The lock file
    /// is derived from this storage's canonical database directory; this API
    /// never creates a second database or a second state authority.
    ///
    /// # Errors
    ///
    /// Rejects a missing root, a closed storage connection, corrupt durable
    /// records, or any unfinished Git side effect that cannot be reconciled.
    pub fn git_candidate_retention(
        &mut self,
        controlled_repository_root: impl AsRef<Path>,
    ) -> Result<CandidateGitRetention<'_>, CandidateGitRetentionError> {
        let controlled_root = fs::canonicalize(controlled_repository_root).map_err(|_| {
            CandidateGitRetentionError::new(
                CandidateGitRetentionErrorKind::NotFound,
                "controlled repository root is unavailable",
            )
        })?;
        if !controlled_root.is_dir() {
            return Err(CandidateGitRetentionError::invalid(
                "controlled repository root is not a directory",
            ));
        }
        let owner_lock_path = self
            .database_path()
            .parent()
            .ok_or_else(|| CandidateGitRetentionError::adapter("database path has no parent"))?
            .join(LOCK_FILE_NAME);
        let connection = self.connection_mut().map_err(|error| {
            CandidateGitRetentionError::new(
                CandidateGitRetentionErrorKind::Closed,
                error.to_string(),
            )
        })?;
        let mut retention = CandidateGitRetention {
            connection,
            controlled_root,
            owner_lock_path,
            git: Box::new(SystemGit),
        };
        retention.with_owner_lock(CandidateGitRetention::reconcile_locked)?;
        Ok(retention)
    }
}

impl CandidateGitRetention<'_> {
    /// Persists a `PinIntent`, creates or verifies the exact stable reference,
    /// and only then advances to `Pinned`.
    ///
    /// The request cannot be built from caller-reported commit fields. It must
    /// combine an opaque source rebuilt by [`crate::GitSourceResolver`] with
    /// the exact complete Artifact write receipt used to form the final ack.
    ///
    /// # Errors
    ///
    /// Rejects an incomplete/foreign acknowledgement, changed replay, missing
    /// Git object, moved repository, foreign ref, corrupt catalog, or adapter
    /// failure. An error after `PinIntent` is durable is recovered on reopen.
    pub fn pin_after_final_artifact_ack(
        &mut self,
        acknowledgement: &ArtifactWriteReceipt,
        source: &ValidatedGitSourceArtifact,
        final_ack_digest: &Sha256Digest,
    ) -> Result<CandidateGitPinReceipt, CandidateGitRetentionError> {
        self.with_owner_lock(|retention| {
            retention.reconcile_locked()?;
            let repository = retention
                .git
                .repository_identity(&retention.controlled_root, source.repository_locator())?;
            let requested = StoredRetention::from_final_ack(
                acknowledgement,
                source,
                final_ack_digest,
                &repository,
            )?;
            let (record, idempotent_replay) = retention.retain_pin_intent(&requested)?;
            if matches!(
                record.state,
                CandidateGitRetentionState::ReleaseIntent | CandidateGitRetentionState::Released
            ) {
                return Ok(record.pin_receipt(true));
            }
            retention
                .git
                .ensure_candidate_reference(&repository, &record)?;
            retention.advance_state(
                &record,
                CandidateGitRetentionState::PinIntent,
                CandidateGitRetentionState::Pinned,
            )?;
            let pinned = load_exact(retention.connection, &record.binding_key)?;
            Ok(pinned.pin_receipt(idempotent_replay))
        })
    }

    /// Commits a stable release receipt before deleting the exact reference.
    ///
    /// # Errors
    ///
    /// Rejects missing/changed pin authority, changed terminal/read-closure
    /// facts, moved repository, a foreign ref target, corrupt durable state,
    /// or adapter failure. An unfinished delete is recovered on reopen.
    pub fn release_after_delivery_final(
        &mut self,
        pin: &CandidateGitPinReceipt,
        authority: &CandidateGitReleaseAuthority,
    ) -> Result<CandidateGitReleaseReceipt, CandidateGitRetentionError> {
        self.with_owner_lock(|retention| {
            retention.reconcile_locked()?;
            let record = load_exact(retention.connection, &binding_from_reference(pin)?)?;
            validate_pin_receipt(&record, pin)?;
            let (release, idempotent_replay) =
                retention.retain_release_intent(&record, authority)?;
            let repository = retention.repository_for_record(&release)?;
            retention
                .git
                .delete_candidate_reference(&repository, &release)?;
            if release.state == CandidateGitRetentionState::ReleaseIntent {
                retention.advance_state(
                    &release,
                    CandidateGitRetentionState::ReleaseIntent,
                    CandidateGitRetentionState::Released,
                )?;
            }
            let released = load_exact(retention.connection, &release.binding_key)?;
            released.release_receipt(idempotent_replay)
        })
    }

    /// Loads one exact retained binding by Artifact identity.
    ///
    /// # Errors
    ///
    /// Rejects malformed input, missing/corrupt rows, or a moved repository.
    pub fn load_by_artifact(
        &mut self,
        artifact_id: &ArtifactId,
    ) -> Result<Option<CandidateGitPinReceipt>, CandidateGitRetentionError> {
        canonical_public_id(&artifact_id.0, "art_", "artifactId")?;
        self.with_owner_lock(|retention| {
            retention.reconcile_locked()?;
            load_by_artifact(retention.connection, &artifact_id.0)
                .map(|record| record.map(|record| record.pin_receipt(true)))
        })
    }

    /// Loads every retained candidate binding for one Delivery in stable
    /// binding-key order.
    ///
    /// The read reconciles unfinished Git side effects first, so a caller can
    /// safely use the returned receipts as the input to the receipt-first
    /// release operation.  Released rows are returned as well: replaying a
    /// terminal release must be able to recover its durable release receipt.
    ///
    /// # Errors
    ///
    /// Rejects malformed Delivery identities, corrupt rows, a moved
    /// repository, or an unfinished Git side effect that cannot be recovered.
    pub fn load_by_delivery(
        &mut self,
        delivery_id: &DeliveryId,
    ) -> Result<Vec<CandidateGitPinReceipt>, CandidateGitRetentionError> {
        canonical_public_id(&delivery_id.0, "dlv_", "deliveryId")?;
        self.with_owner_lock(|retention| {
            retention.reconcile_locked()?;
            load_by_delivery(retention.connection, &delivery_id.0).map(|records| {
                records
                    .into_iter()
                    .map(|record| record.pin_receipt(true))
                    .collect()
            })
        })
    }

    fn retain_pin_intent(
        &mut self,
        requested: &StoredRetention,
    ) -> Result<(StoredRetention, bool), CandidateGitRetentionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_adapter)?;
        if let Some(existing) = load_by_artifact_transaction(&transaction, &requested.artifact_id)?
        {
            if pin_identity(&existing) != pin_identity(requested) {
                return Err(CandidateGitRetentionError::conflict(
                    "candidate Artifact is already bound to another Git retention",
                ));
            }
            transaction.commit().map_err(sql_adapter)?;
            return Ok((existing, true));
        }
        if load_record_transaction(&transaction, &requested.binding_key)?.is_some() {
            return Err(CandidateGitRetentionError::conflict(
                "candidate Git binding key is already used by another Artifact",
            ));
        }
        insert_record(&transaction, requested)?;
        transaction.commit().map_err(sql_adapter)?;
        Ok((requested.clone(), false))
    }

    fn retain_release_intent(
        &mut self,
        record: &StoredRetention,
        authority: &CandidateGitReleaseAuthority,
    ) -> Result<(StoredRetention, bool), CandidateGitRetentionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_adapter)?;
        let current =
            load_record_transaction(&transaction, &record.binding_key)?.ok_or_else(|| {
                CandidateGitRetentionError::new(
                    CandidateGitRetentionErrorKind::NotFound,
                    "candidate Git retention does not exist",
                )
            })?;
        if pin_identity(&current) != pin_identity(record) {
            return Err(CandidateGitRetentionError::conflict(
                "candidate Git pin authority changed before release",
            ));
        }
        match current.state {
            CandidateGitRetentionState::PinIntent => Err(CandidateGitRetentionError::conflict(
                "candidate Git reference is not pinned",
            )),
            CandidateGitRetentionState::Pinned => {
                if authority.delivery_id.0 != current.delivery_id {
                    return Err(CandidateGitRetentionError::conflict(
                        "candidate Git release Delivery differs from its Artifact authority",
                    ));
                }
                let released = current.with_release(authority)?;
                update_record(&transaction, &released, CandidateGitRetentionState::Pinned)?;
                transaction.commit().map_err(sql_adapter)?;
                Ok((released, false))
            }
            CandidateGitRetentionState::ReleaseIntent | CandidateGitRetentionState::Released => {
                if authority.delivery_id.0 != current.delivery_id {
                    return Err(CandidateGitRetentionError::conflict(
                        "candidate Git release Delivery differs from its Artifact authority",
                    ));
                }
                let expected = current.clone().with_release(authority)?;
                if current.release != expected.release {
                    return Err(CandidateGitRetentionError::conflict(
                        "candidate Git release authority changed on replay",
                    ));
                }
                transaction.commit().map_err(sql_adapter)?;
                Ok((current, true))
            }
        }
    }

    fn advance_state(
        &mut self,
        record: &StoredRetention,
        expected: CandidateGitRetentionState,
        next: CandidateGitRetentionState,
    ) -> Result<(), CandidateGitRetentionError> {
        let mut next_record = record.clone();
        next_record.state = next;
        next_record.validate()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_adapter)?;
        let current =
            load_record_transaction(&transaction, &record.binding_key)?.ok_or_else(|| {
                CandidateGitRetentionError::corrupt("candidate Git retention disappeared")
            })?;
        if current == next_record {
            transaction.commit().map_err(sql_adapter)?;
            return Ok(());
        }
        if current != *record || current.state != expected {
            return Err(CandidateGitRetentionError::conflict(
                "candidate Git retention changed during side effect",
            ));
        }
        update_record(&transaction, &next_record, expected)?;
        transaction.commit().map_err(sql_adapter)
    }

    fn reconcile_locked(&mut self) -> Result<(), CandidateGitRetentionError> {
        let records = load_all(self.connection)?;
        for record in records {
            let repository = self.repository_for_record(&record)?;
            match record.state {
                CandidateGitRetentionState::PinIntent => {
                    self.git.ensure_candidate_reference(&repository, &record)?;
                    self.advance_state(
                        &record,
                        CandidateGitRetentionState::PinIntent,
                        CandidateGitRetentionState::Pinned,
                    )?;
                }
                CandidateGitRetentionState::Pinned => {
                    self.git.ensure_candidate_reference(&repository, &record)?;
                }
                CandidateGitRetentionState::ReleaseIntent => {
                    self.git.delete_candidate_reference(&repository, &record)?;
                    self.advance_state(
                        &record,
                        CandidateGitRetentionState::ReleaseIntent,
                        CandidateGitRetentionState::Released,
                    )?;
                }
                CandidateGitRetentionState::Released => {
                    self.git.delete_candidate_reference(&repository, &record)?;
                }
            }
        }
        Ok(())
    }

    fn repository_for_record(
        &self,
        record: &StoredRetention,
    ) -> Result<RepositoryIdentity, CandidateGitRetentionError> {
        let repository = self
            .git
            .repository_identity(&self.controlled_root, &record.repository_locator)?;
        if path_text(&repository.repository, "repository path")? != record.repository_path
            || path_text(&repository.git_common_directory, "Git directory")?
                != record.git_common_directory
        {
            return Err(CandidateGitRetentionError::new(
                CandidateGitRetentionErrorKind::PermissionDenied,
                "controlled repository identity changed after candidate retention",
            ));
        }
        Ok(repository)
    }

    fn with_owner_lock<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, CandidateGitRetentionError>,
    ) -> Result<T, CandidateGitRetentionError> {
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.owner_lock_path)
            .map_err(io_adapter)?;
        lock.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => CandidateGitRetentionError::conflict(
                "candidate Git retention is still owned by another process",
            ),
            fs::TryLockError::Error(error) => io_adapter(error),
        })?;
        let result = operation(self);
        let unlock = lock.unlock().map_err(io_adapter);
        match (result, unlock) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }
}

pub(crate) fn create_schema(transaction: &Transaction<'_>) -> Result<(), StorageError> {
    transaction.execute_batch(SCHEMA).map_err(sql_error)
}

pub(crate) fn validate_schema(transaction: &Transaction<'_>) -> Result<(), StorageError> {
    transaction
        .prepare(
            "SELECT binding_key, artifact_id, reference_name, state, record_json \
             FROM git_candidate_retentions LIMIT 0",
        )
        .map(|_| ())
        .map_err(sql_error)
}

fn insert_record(
    transaction: &Transaction<'_>,
    record: &StoredRetention,
) -> Result<(), CandidateGitRetentionError> {
    let bytes = encode_record(record)?;
    transaction
        .execute(
            "INSERT INTO git_candidate_retentions \
             (binding_key, artifact_id, reference_name, state, record_json) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.binding_key,
                record.artifact_id,
                record.reference_name,
                record.state.as_str(),
                bytes,
            ],
        )
        .map_err(sql_conflict)?;
    Ok(())
}

fn update_record(
    transaction: &Transaction<'_>,
    record: &StoredRetention,
    expected: CandidateGitRetentionState,
) -> Result<(), CandidateGitRetentionError> {
    let bytes = encode_record(record)?;
    let changed = transaction
        .execute(
            "UPDATE git_candidate_retentions SET state = ?2, record_json = ?3 \
             WHERE binding_key = ?1 AND state = ?4",
            params![
                record.binding_key,
                record.state.as_str(),
                bytes,
                expected.as_str(),
            ],
        )
        .map_err(sql_adapter)?;
    if changed != 1 {
        return Err(CandidateGitRetentionError::conflict(
            "candidate Git retention state compare-and-swap failed",
        ));
    }
    Ok(())
}

fn load_all(connection: &Connection) -> Result<Vec<StoredRetention>, CandidateGitRetentionError> {
    let mut statement = connection
        .prepare(
            "SELECT binding_key, artifact_id, reference_name, state, record_json \
             FROM git_candidate_retentions ORDER BY binding_key ASC",
        )
        .map_err(sql_adapter)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })
        .map_err(sql_adapter)?;
    let rows = rows.collect::<Result<Vec<_>, _>>().map_err(sql_adapter)?;
    rows.into_iter()
        .map(|row| decode_row(&row.0, &row.1, &row.2, &row.3, &row.4))
        .collect()
}

fn load_exact(
    connection: &Connection,
    binding_key: &str,
) -> Result<StoredRetention, CandidateGitRetentionError> {
    load_record_connection(connection, binding_key)?.ok_or_else(|| {
        CandidateGitRetentionError::new(
            CandidateGitRetentionErrorKind::NotFound,
            "candidate Git retention does not exist",
        )
    })
}

fn load_by_artifact(
    connection: &Connection,
    artifact_id: &str,
) -> Result<Option<StoredRetention>, CandidateGitRetentionError> {
    let row = connection
        .query_row(
            "SELECT binding_key, artifact_id, reference_name, state, record_json \
             FROM git_candidate_retentions WHERE artifact_id = ?1",
            [artifact_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_adapter)?;
    row.map(|row| decode_row(&row.0, &row.1, &row.2, &row.3, &row.4))
        .transpose()
}

fn load_by_delivery(
    connection: &Connection,
    delivery_id: &str,
) -> Result<Vec<StoredRetention>, CandidateGitRetentionError> {
    Ok(load_all(connection)?
        .into_iter()
        .filter(|record| record.delivery_id == delivery_id)
        .collect())
}

fn load_by_artifact_transaction(
    transaction: &Transaction<'_>,
    artifact_id: &str,
) -> Result<Option<StoredRetention>, CandidateGitRetentionError> {
    let row = transaction
        .query_row(
            "SELECT binding_key, artifact_id, reference_name, state, record_json \
             FROM git_candidate_retentions WHERE artifact_id = ?1",
            [artifact_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_adapter)?;
    row.map(|row| decode_row(&row.0, &row.1, &row.2, &row.3, &row.4))
        .transpose()
}

fn load_record_connection(
    connection: &Connection,
    binding_key: &str,
) -> Result<Option<StoredRetention>, CandidateGitRetentionError> {
    let row = connection
        .query_row(
            "SELECT binding_key, artifact_id, reference_name, state, record_json \
             FROM git_candidate_retentions WHERE binding_key = ?1",
            [binding_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_adapter)?;
    row.map(|row| decode_row(&row.0, &row.1, &row.2, &row.3, &row.4))
        .transpose()
}

fn load_record_transaction(
    transaction: &Transaction<'_>,
    binding_key: &str,
) -> Result<Option<StoredRetention>, CandidateGitRetentionError> {
    let row = transaction
        .query_row(
            "SELECT binding_key, artifact_id, reference_name, state, record_json \
             FROM git_candidate_retentions WHERE binding_key = ?1",
            [binding_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_adapter)?;
    row.map(|row| decode_row(&row.0, &row.1, &row.2, &row.3, &row.4))
        .transpose()
}

fn decode_row(
    binding_key: &str,
    artifact_id: &str,
    reference_name: &str,
    state: &str,
    bytes: &[u8],
) -> Result<StoredRetention, CandidateGitRetentionError> {
    let record: StoredRetention = serde_json::from_slice(bytes).map_err(|_| {
        CandidateGitRetentionError::corrupt("candidate Git retention record is invalid")
    })?;
    record.validate()?;
    let canonical = encode_record(&record)?;
    if canonical != bytes
        || record.binding_key != binding_key
        || record.artifact_id != artifact_id
        || record.reference_name != reference_name
        || record.state != CandidateGitRetentionState::parse(state)?
    {
        return Err(CandidateGitRetentionError::corrupt(
            "candidate Git retention row differs from its canonical record",
        ));
    }
    Ok(record)
}

fn encode_record(record: &StoredRetention) -> Result<Vec<u8>, CandidateGitRetentionError> {
    record.validate()?;
    serde_json::to_vec(record).map_err(|_| {
        CandidateGitRetentionError::adapter("candidate Git retention record cannot be encoded")
    })
}

fn verify_candidate_objects(
    repository: &RepositoryIdentity,
    record: &StoredRetention,
) -> Result<(), CandidateGitRetentionError> {
    let commit = git_text(
        &repository.repository,
        &[
            "rev-parse".into(),
            "--verify".into(),
            format!("{}^{{commit}}", record.candidate_commit_id).into(),
        ],
    )?;
    let tree = git_text(
        &repository.repository,
        &[
            "rev-parse".into(),
            "--verify".into(),
            format!("{}^{{tree}}", record.candidate_commit_id).into(),
        ],
    )?;
    if commit != record.candidate_commit_id || tree != record.candidate_tree_id {
        return Err(CandidateGitRetentionError::conflict(
            "candidate Git object differs from acknowledged source facts",
        ));
    }
    Ok(())
}

fn reference_target(
    repository: &Path,
    reference_name: &str,
) -> Result<Option<String>, CandidateGitRetentionError> {
    let output = git_status(
        repository,
        &[
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            reference_name.into(),
        ],
    )?;
    if output.status.success() {
        return git_stdout(output).map(Some);
    }
    if output.status.code() == Some(1) && output.stdout.is_empty() {
        return Ok(None);
    }
    Err(CandidateGitRetentionError::adapter(
        "candidate Git reference cannot be read",
    ))
}

fn git_text(
    repository: &Path,
    arguments: &[OsString],
) -> Result<String, CandidateGitRetentionError> {
    let output = git_status(repository, arguments)?;
    if !output.status.success() {
        return Err(CandidateGitRetentionError::new(
            CandidateGitRetentionErrorKind::NotFound,
            "required Git source fact is unavailable",
        ));
    }
    git_stdout(output)
}

fn git_stdout(output: Output) -> Result<String, CandidateGitRetentionError> {
    let value = String::from_utf8(output.stdout)
        .map_err(|_| CandidateGitRetentionError::corrupt("Git output is not UTF-8"))?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    if value.is_empty() || value.contains(['\r', '\n']) {
        return Err(CandidateGitRetentionError::corrupt(
            "Git output is not one exact value",
        ));
    }
    Ok(value.to_owned())
}

fn git_status(
    repository: &Path,
    arguments: &[OsString],
) -> Result<Output, CandidateGitRetentionError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|_| CandidateGitRetentionError::adapter("Git command failed"))
}

fn pin_identity(
    record: &StoredRetention,
) -> (&str, &str, &str, &str, &str, &str, &str, &str, &str) {
    (
        &record.binding_key,
        &record.artifact_id,
        &record.artifact_digest,
        &record.delivery_id,
        &record.repository_locator,
        &record.git_common_directory,
        &record.candidate_commit_id,
        &record.candidate_tree_id,
        &record.final_ack_digest,
    )
}

fn validate_pin_receipt(
    record: &StoredRetention,
    receipt: &CandidateGitPinReceipt,
) -> Result<(), CandidateGitRetentionError> {
    if receipt.artifact_id.0 != record.artifact_id
        || receipt.artifact_digest.0 != record.artifact_digest
        || receipt.delivery_id.0 != record.delivery_id
        || receipt.repository_locator != record.repository_locator
        || receipt.candidate_commit_id != record.candidate_commit_id
        || receipt.candidate_tree_id != record.candidate_tree_id
        || receipt.reference_name != record.reference_name
        || receipt.receipt_digest.0 != record.pin_receipt_digest
    {
        return Err(CandidateGitRetentionError::conflict(
            "candidate Git pin receipt differs from durable authority",
        ));
    }
    Ok(())
}

fn binding_from_reference(
    receipt: &CandidateGitPinReceipt,
) -> Result<String, CandidateGitRetentionError> {
    let binding = receipt
        .reference_name
        .strip_prefix(REFERENCE_PREFIX)
        .ok_or_else(|| CandidateGitRetentionError::invalid("candidate reference is invalid"))?;
    sha256_hex(binding, "candidateBindingKey")?;
    Ok(binding.to_owned())
}

fn binding_key(
    artifact_id: &str,
    artifact_digest: &str,
    repository_locator: &str,
    git_common_directory: &str,
    candidate_commit_id: &str,
    candidate_tree_id: &str,
) -> String {
    digest_fields_hex(
        b"winwincode.git-candidate-binding.v1",
        &[
            artifact_id,
            artifact_digest,
            repository_locator,
            git_common_directory,
            candidate_commit_id,
            candidate_tree_id,
        ],
    )
}

fn pin_receipt_digest(binding_key: &str, reference_name: &str, final_ack_digest: &str) -> String {
    digest_fields(
        b"winwincode.git-candidate-pin-receipt.v1",
        &[binding_key, reference_name, final_ack_digest],
    )
}

fn release_receipt_digest(
    binding_key: &str,
    pin_receipt_digest: &str,
    delivery_id: &str,
    outcome: CandidateGitTerminalOutcome,
    terminal_receipt_digest: &str,
    reads_closed_receipt_digest: &str,
) -> String {
    let outcome = match outcome {
        CandidateGitTerminalOutcome::Delivered => "delivered",
        CandidateGitTerminalOutcome::Rejected => "rejected",
        CandidateGitTerminalOutcome::Cancelled => "cancelled",
    };
    digest_fields(
        b"winwincode.git-candidate-release-receipt.v1",
        &[
            binding_key,
            pin_receipt_digest,
            delivery_id,
            outcome,
            terminal_receipt_digest,
            reads_closed_receipt_digest,
        ],
    )
}

fn digest_fields(domain: &[u8], fields: &[&str]) -> String {
    format!("sha256:{}", digest_fields_hex(domain, fields))
}

fn digest_fields_hex(domain: &[u8], fields: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for field in fields {
        hasher.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn portable_locator(value: &str) -> Result<(), CandidateGitRetentionError> {
    if value.is_empty()
        || value.len() > MAX_LOCATOR_BYTES
        || Path::new(value).is_absolute()
        || value.contains('\\')
        || value.bytes().any(|byte| byte <= 31 || byte == 127)
        || value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(CandidateGitRetentionError::invalid(
            "repository locator is not a portable controlled relative path",
        ));
    }
    Ok(())
}

fn absolute_path(value: &str, field: &str) -> Result<(), CandidateGitRetentionError> {
    if value.is_empty()
        || value.len() > MAX_LOCATOR_BYTES * 4
        || !Path::new(value).is_absolute()
        || value
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r')
    {
        return Err(CandidateGitRetentionError::corrupt(format!(
            "stored {field} is invalid"
        )));
    }
    Ok(())
}

fn path_text(path: &Path, field: &str) -> Result<String, CandidateGitRetentionError> {
    let value = path.to_str().ok_or_else(|| {
        CandidateGitRetentionError::invalid(format!("{field} is not valid UTF-8"))
    })?;
    absolute_path(value, field)?;
    Ok(value.to_owned())
}

fn git_object_id(value: &str, field: &str) -> Result<(), CandidateGitRetentionError> {
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(CandidateGitRetentionError::invalid(format!(
            "{field} is not a lowercase Git object identity"
        )));
    }
    Ok(())
}

fn zero_object_id(length: usize) -> Result<&'static str, CandidateGitRetentionError> {
    match length {
        40 => Ok(ZERO_SHA1),
        64 => Ok(ZERO_SHA256),
        _ => Err(CandidateGitRetentionError::invalid(
            "candidate Git object format is unsupported",
        )),
    }
}

fn sha256_digest(value: &str, field: &str) -> Result<(), CandidateGitRetentionError> {
    let Some(value) = value.strip_prefix("sha256:") else {
        return Err(CandidateGitRetentionError::invalid(format!(
            "{field} is not a canonical SHA-256 digest"
        )));
    };
    if sha256_hex(value, field).is_err() {
        return Err(CandidateGitRetentionError::invalid(format!(
            "{field} is not a canonical SHA-256 digest"
        )));
    }
    Ok(())
}

fn sha256_hex(value: &str, field: &str) -> Result<(), CandidateGitRetentionError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(CandidateGitRetentionError::invalid(format!(
            "{field} is not lowercase SHA-256 hex"
        )));
    }
    Ok(())
}

fn canonical_public_id(
    value: &str,
    prefix: &str,
    field: &str,
) -> Result<(), CandidateGitRetentionError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(CandidateGitRetentionError::invalid(format!(
            "{field} is not canonical"
        )));
    };
    if suffix.len() != 26
        || value.len() > MAX_ID_BYTES
        || !suffix.bytes().all(|byte| {
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
        })
    {
        return Err(CandidateGitRetentionError::invalid(format!(
            "{field} is not canonical"
        )));
    }
    Ok(())
}

fn sql_adapter(_error: rusqlite::Error) -> CandidateGitRetentionError {
    CandidateGitRetentionError::adapter("candidate Git retention database operation failed")
}

fn sql_conflict(error: rusqlite::Error) -> CandidateGitRetentionError {
    if matches!(error, rusqlite::Error::SqliteFailure(_, _)) {
        CandidateGitRetentionError::conflict("candidate Git retention identity already exists")
    } else {
        sql_adapter(error)
    }
}

fn io_adapter(_error: std::io::Error) -> CandidateGitRetentionError {
    CandidateGitRetentionError::adapter("candidate Git retention lock operation failed")
}
