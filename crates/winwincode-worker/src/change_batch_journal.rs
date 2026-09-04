// SPDX-License-Identifier: Apache-2.0

//! Worker-private durable state for deterministic `ChangeBatch` execution.
//!
//! The Composer's Codex record proves that a proposal was produced. This
//! journal separately proves what the sole Writer authorized, which preimages
//! were durable before the first mutation, how far apply advanced, and which
//! progress and receipt values still need acknowledgement.

use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use winwincode_change_batch::{
    ChangeBatchPolicy, ExecutionJournalError, ExecutionJournalPort, FilePreimage,
    MAX_PREIMAGE_BYTES, PreparedChangeBatchPlan, PreparedPreimageJournalRecord,
    canonical_applied_file_summaries, derive_delta_digest, prepare_change_batch,
    rebuild_preimage_journal_record, validate_preimage_journal_record,
};
use winwincode_domain::{ChangeBatchId, Instant, Sha256Digest, WorkspaceRevision};
use winwincode_execution_port::{
    change_batch_identity::validate_change_batch_identity_derivation,
    change_batch_progress::ChangeBatchProgressLedger,
    diagnostic_parser::{
        DiagnosticParseBatch, validate_diagnostic_baseline, validate_diagnostic_baseline_comparison,
    },
    generated::{
        ChangeBatchIdentity, ChangeBatchProgressEvent, ChangeBatchProgressState,
        ChangeBatchProposalEvent, ChangeBatchReceipt, ChangeBatchReceiptStatus, DiagnosticBaseline,
        DiagnosticBaselineComparison, DiagnosticParserVersion, NormalizedDiagnostic,
        NormalizerReceipt, ValidationProfileSelection, ValidationReceipt,
    },
    validation_config::{validate_normalizer_receipt_binding, validate_validation_receipt_binding},
};

use crate::validation_diagnostics::{
    ValidationDiagnosticDisposition, decide_validation_diagnostics,
};
use crate::workspace_phase::PhaseProcessReceipt;
use crate::workspace_tree::{
    WorkspaceTreeError, WorkspaceTreeRestoreIntent, WorkspaceTreeRestoreJournalPort,
};

/// Hard limit for all rollback preimages retained by one batch.
pub const MAX_CHANGE_BATCH_PREIMAGE_BYTES: u64 = MAX_PREIMAGE_BYTES;

const DATABASE_FILE: &str = "change-batch.sqlite3";
const BLOB_DIRECTORY: &str = "preimages";
const MAX_RECOVERY_RECORDS: usize = 1_024;
const JOURNAL_SCHEMA_VERSION: i64 = 4;

/// Stable Worker journal failure category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeBatchJournalErrorCode {
    Invalid,
    Conflict,
    Corrupt,
    Capacity,
    Unavailable,
}

/// Secret-free Worker journal failure.
#[derive(Debug)]
pub struct ChangeBatchJournalError {
    code: ChangeBatchJournalErrorCode,
    message: &'static str,
}

impl ChangeBatchJournalError {
    const fn new(code: ChangeBatchJournalErrorCode, message: &'static str) -> Self {
        Self { code, message }
    }

    /// Returns the stable machine-readable category.
    #[must_use]
    pub const fn code(&self) -> ChangeBatchJournalErrorCode {
        self.code
    }

    /// Returns the bounded secret-free failure description.
    #[must_use]
    pub const fn safe_message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ChangeBatchJournalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ChangeBatchJournalError {}

/// Durable apply phase owned by the Worker rather than the Composer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeBatchExecutionPhase {
    IntentRetained,
    PreimagesReady,
    Applying,
    Applied,
    RollbackRequired,
    RolledBack,
    StateUncertain,
}

/// Workspace-scoped single-Writer barrier around one active `ChangeBatch`.
///
/// `Idle` and `Accepted` have no active batch. Every other state retains the
/// batch id until exact rollback, repair routing, or operator reconciliation
/// makes the checkout safe to release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveBatchState {
    Idle,
    Applying,
    CheckpointPending,
    Checkpointed,
    ValidationPending,
    ObservationPending,
    Accepted,
    RollbackPending,
    RolledBack,
    RepairRequired,
    Quarantined,
}

impl ActiveBatchState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Applying => "applying",
            Self::CheckpointPending => "checkpoint_pending",
            Self::Checkpointed => "checkpointed",
            Self::ValidationPending => "validation_pending",
            Self::ObservationPending => "observation_pending",
            Self::Accepted => "accepted",
            Self::RollbackPending => "rollback_pending",
            Self::RolledBack => "rolled_back",
            Self::RepairRequired => "repair_required",
            Self::Quarantined => "quarantined",
        }
    }

    fn parse(value: &str) -> Result<Self, ChangeBatchJournalError> {
        match value {
            "idle" => Ok(Self::Idle),
            "applying" => Ok(Self::Applying),
            "checkpoint_pending" => Ok(Self::CheckpointPending),
            "checkpointed" => Ok(Self::Checkpointed),
            "validation_pending" => Ok(Self::ValidationPending),
            "observation_pending" => Ok(Self::ObservationPending),
            "accepted" => Ok(Self::Accepted),
            "rollback_pending" => Ok(Self::RollbackPending),
            "rolled_back" => Ok(Self::RolledBack),
            "repair_required" => Ok(Self::RepairRequired),
            "quarantined" => Ok(Self::Quarantined),
            _ => Err(corrupt("ChangeBatch workspace barrier state is unknown")),
        }
    }

    const fn has_active_batch(self) -> bool {
        !matches!(self, Self::Idle | Self::Accepted)
    }

    pub(crate) const fn has_unresolved_mutation(self) -> bool {
        matches!(
            self,
            Self::Applying | Self::CheckpointPending | Self::RollbackPending | Self::Quarantined
        )
    }
}

/// Revalidated durable barrier state for one checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceBatchBarrier {
    pub workspace_id: String,
    pub accepted_revision: WorkspaceRevision,
    pub active_batch_id: Option<ChangeBatchId>,
    pub state: ActiveBatchState,
    pub checkpoint_revision: Option<WorkspaceRevision>,
    pub checkpoint_delta_digest: Option<Sha256Digest>,
}

/// Result of a revision-gated observation transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationGateResult {
    Accepted,
    Stale,
}

/// Revalidated Writer/Validation durable state for one active batch.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeBatchPhaseRecord {
    pub selection: ValidationProfileSelection,
    pub command_receipts: Vec<PhaseProcessReceipt>,
    pub diagnostic_batches: Vec<Option<DiagnosticParseBatch>>,
    pub diagnostic_parse_failures: Vec<bool>,
    pub normalizer_receipt: Option<NormalizerReceipt>,
    pub validation_receipt: Option<ValidationReceipt>,
}

/// Durable deterministic decision over one exact base/result diagnostic pair.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ValidationDiagnosticEvaluation {
    pub base_revision: WorkspaceRevision,
    pub result_revision: WorkspaceRevision,
    pub baseline: Option<DiagnosticBaseline>,
    pub result: Option<DiagnosticBaseline>,
    pub comparison: Option<DiagnosticBaselineComparison>,
    pub parser_failed: bool,
    pub disposition: String,
    pub reason_code: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticScope<'selection> {
    configuration_digest: &'selection Sha256Digest,
    profile: &'selection winwincode_execution_port::generated::ValidationProfileName,
    command_ids: &'selection [String],
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct PersistedDiagnosticParseBatch {
    parser_version: DiagnosticParserVersion,
    diagnostics: Vec<NormalizedDiagnostic>,
}

impl From<&DiagnosticParseBatch> for PersistedDiagnosticParseBatch {
    fn from(value: &DiagnosticParseBatch) -> Self {
        Self {
            parser_version: value.parser_version.clone(),
            diagnostics: value.diagnostics.clone(),
        }
    }
}

impl From<PersistedDiagnosticParseBatch> for DiagnosticParseBatch {
    fn from(value: PersistedDiagnosticParseBatch) -> Self {
        Self {
            parser_version: value.parser_version,
            diagnostics: value.diagnostics,
        }
    }
}

impl ChangeBatchExecutionPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::IntentRetained => "intent_retained",
            Self::PreimagesReady => "preimages_ready",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::RollbackRequired => "rollback_required",
            Self::RolledBack => "rolled_back",
            Self::StateUncertain => "state_uncertain",
        }
    }

    fn parse(value: &str) -> Result<Self, ChangeBatchJournalError> {
        match value {
            "intent_retained" => Ok(Self::IntentRetained),
            "preimages_ready" => Ok(Self::PreimagesReady),
            "applying" => Ok(Self::Applying),
            "applied" => Ok(Self::Applied),
            "rollback_required" => Ok(Self::RollbackRequired),
            "rolled_back" => Ok(Self::RolledBack),
            "state_uncertain" => Ok(Self::StateUncertain),
            _ => Err(corrupt("ChangeBatch journal phase is unknown")),
        }
    }
}

/// Result of retaining an immutable value under an idempotency key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalRetention {
    Inserted,
    Replay,
}

/// Persisted execution facts for one batch.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeBatchJournalRecord {
    pub event: ChangeBatchProposalEvent,
    pub intent_digest: Sha256Digest,
    pub proposal_digest: Sha256Digest,
    pub authority_digest: Sha256Digest,
    pub base_revision: WorkspaceRevision,
    pub plan_digest: Sha256Digest,
    pub phase: ChangeBatchExecutionPhase,
    pub next_operation: u64,
    pub receipt: Option<ChangeBatchReceipt>,
}

/// Exact file state used for crash recovery classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileStateFingerprint {
    pub exists: bool,
    pub digest: Option<Sha256Digest>,
    pub mode: Option<String>,
}

impl FileStateFingerprint {
    fn validate(&self) -> Result<(), ChangeBatchJournalError> {
        if self.exists != self.digest.is_some()
            || (!self.exists && self.mode.is_some())
            || self
                .digest
                .as_ref()
                .is_some_and(|digest| !valid_digest(digest))
            || self.mode.as_deref().is_some_and(str::is_empty)
        {
            return Err(invalid("ChangeBatch file fingerprint is invalid"));
        }
        Ok(())
    }
}

/// One exact rollback preimage and the expected state after its operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RollbackPreimage {
    pub ordinal: u64,
    pub path: String,
    pub operation: String,
    pub before_bytes: Option<Vec<u8>>,
    pub before_mode: Option<String>,
    pub after: FileStateFingerprint,
}

/// Three-way recovery classification for a possibly interrupted operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeBatchRecoveryState {
    Before,
    After,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRollbackEntry {
    ordinal: u64,
    path: String,
    operation: String,
    before_exists: bool,
    before_digest: Option<Sha256Digest>,
    before_len: u64,
    before_mode: Option<String>,
    after: StoredFileState,
    blob_digest: Option<Sha256Digest>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredFileState {
    exists: bool,
    digest: Option<Sha256Digest>,
    mode: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredMutationPreimageManifest {
    batch_id: ChangeBatchId,
    plan_digest: Sha256Digest,
    preimage_digest: Sha256Digest,
    total_preimage_bytes: u64,
    files: Vec<StoredMutationPreimage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredMutationPreimage {
    path: String,
    digest: Option<Sha256Digest>,
    mode: Option<String>,
    expected_after_digest: Option<Sha256Digest>,
    expected_after_mode: Option<String>,
    byte_length: u64,
}

type PreparedRollbackEntries = Vec<(Option<Vec<u8>>, StoredRollbackEntry)>;

impl From<&FileStateFingerprint> for StoredFileState {
    fn from(value: &FileStateFingerprint) -> Self {
        Self {
            exists: value.exists,
            digest: value.digest.clone(),
            mode: value.mode.clone(),
        }
    }
}

/// SQLite-backed Worker journal with a private content-addressed preimage root.
pub struct ChangeBatchJournal {
    connection: Connection,
    blob_root: PathBuf,
}

impl fmt::Debug for ChangeBatchJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChangeBatchJournal")
            .finish_non_exhaustive()
    }
}

impl ChangeBatchJournal {
    /// Opens the private journal with FULL synchronous WAL durability.
    ///
    /// # Errors
    ///
    /// Rejects linked or unavailable state paths and corrupt schema state.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ChangeBatchJournalError> {
        ensure_private_directory(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref())
            .map_err(|_| unavailable("ChangeBatch journal root cannot be opened"))?;
        let blob_root = root.join(BLOB_DIRECTORY);
        ensure_private_directory(&blob_root)?;
        let database = root.join(DATABASE_FILE);
        ensure_private_file(&database)?;
        let mut connection = Connection::open(&database)
            .map_err(|_| unavailable("ChangeBatch journal database cannot be opened"))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(|_| unavailable("ChangeBatch journal durability cannot be configured"))?;
        migrate_journal_schema(&mut connection)?;
        Ok(Self {
            connection,
            blob_root,
        })
    }

    /// Creates or replays the workspace's durable accepted-revision anchor.
    ///
    /// # Errors
    ///
    /// Rejects an empty identity/revision or a changed initial revision.
    pub fn retain_workspace_barrier(
        &mut self,
        workspace_id: &str,
        accepted_revision: &WorkspaceRevision,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        if workspace_id.is_empty()
            || !valid_workspace_revision(accepted_revision)
            || now.0.is_empty()
        {
            return Err(invalid("ChangeBatch workspace barrier identity is invalid"));
        }
        if let Some(existing) = self.workspace_barrier(workspace_id)? {
            if existing.state == ActiveBatchState::Idle
                && existing.accepted_revision != *accepted_revision
            {
                return Err(conflict(
                    "ChangeBatch workspace initial revision changed on replay",
                ));
            }
            return Ok(JournalRetention::Replay);
        }
        self.connection
            .execute(
                "INSERT INTO change_batch_workspace_barrier (
                   workspace_id, accepted_revision, active_batch_id, state,
                   checkpoint_revision, checkpoint_delta_digest, updated_at
                 ) VALUES (?1, ?2, NULL, ?3, NULL, NULL, ?4)",
                params![
                    workspace_id,
                    accepted_revision.0,
                    ActiveBatchState::Idle.as_str(),
                    now.0
                ],
            )
            .map_err(|_| unavailable("ChangeBatch workspace barrier cannot be persisted"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Returns one revalidated workspace barrier.
    ///
    /// # Errors
    ///
    /// Rejects corrupt or internally inconsistent durable state.
    pub fn workspace_barrier(
        &self,
        workspace_id: &str,
    ) -> Result<Option<WorkspaceBatchBarrier>, ChangeBatchJournalError> {
        load_workspace_barrier_row(&self.connection, workspace_id)?
            .map(|row| workspace_barrier_from_row(workspace_id, row))
            .transpose()
    }

    /// Claims the workspace's single-Writer barrier for an exact retained intent.
    ///
    /// # Errors
    ///
    /// Rejects another non-terminal batch, a stale base, or changed replay.
    pub fn claim_workspace_batch(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProposalEvent,
        expected_base_revision: &WorkspaceRevision,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        validate_event(event)?;
        let barrier = self
            .workspace_barrier(workspace_id)?
            .ok_or_else(|| conflict("ChangeBatch workspace barrier does not exist"))?;
        if barrier.accepted_revision != *expected_base_revision {
            return Err(conflict("ChangeBatch workspace base revision is stale"));
        }
        if barrier.active_batch_id.as_ref() == Some(&event.identity.batch_id) {
            return Ok(JournalRetention::Replay);
        }
        if barrier.active_batch_id.is_some()
            || !matches!(
                barrier.state,
                ActiveBatchState::Idle | ActiveBatchState::Accepted
            )
        {
            return Err(conflict(
                "ChangeBatch workspace already has an active batch",
            ));
        }
        let changed = self
            .connection
            .execute(
                "UPDATE change_batch_workspace_barrier
                 SET active_batch_id = ?2, state = ?3,
                     checkpoint_revision = NULL, checkpoint_delta_digest = NULL,
                     updated_at = ?4
                 WHERE workspace_id = ?1 AND active_batch_id IS NULL
                   AND accepted_revision = ?5 AND state IN ('idle', 'accepted')",
                params![
                    workspace_id,
                    event.identity.batch_id.0,
                    ActiveBatchState::Applying.as_str(),
                    now.0,
                    expected_base_revision.0,
                ],
            )
            .map_err(|_| unavailable("ChangeBatch workspace barrier cannot be claimed"))?;
        if changed != 1 {
            return Err(conflict(
                "ChangeBatch workspace barrier compare-and-set failed",
            ));
        }
        Ok(JournalRetention::Inserted)
    }

    /// Moves one active workspace barrier through an exact legal state edge.
    ///
    /// # Errors
    ///
    /// Rejects a foreign batch, stale state, or illegal edge.
    pub fn transition_workspace_batch(
        &mut self,
        workspace_id: &str,
        batch_id: &ChangeBatchId,
        expected: ActiveBatchState,
        next: ActiveBatchState,
        now: &Instant,
    ) -> Result<(), ChangeBatchJournalError> {
        if !legal_workspace_transition(expected, next) || now.0.is_empty() {
            return Err(invalid("ChangeBatch workspace transition is invalid"));
        }
        let changed = self
            .connection
            .execute(
                "UPDATE change_batch_workspace_barrier SET state = ?4, updated_at = ?5
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = ?3",
                params![
                    workspace_id,
                    batch_id.0,
                    expected.as_str(),
                    next.as_str(),
                    now.0
                ],
            )
            .map_err(|_| unavailable("ChangeBatch workspace transition cannot be persisted"))?;
        if changed != 1 {
            return Err(conflict(
                "ChangeBatch workspace transition compare-and-set failed",
            ));
        }
        Ok(())
    }

    /// Atomically appends one canonical progress fact and moves its workspace barrier.
    ///
    /// # Errors
    ///
    /// Rejects a foreign batch, stale state, changed replay, or a progress fact
    /// that does not describe the requested barrier edge.
    pub fn retain_workspace_progress(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProgressEvent,
        expected: ActiveBatchState,
        next: ActiveBatchState,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        if !valid_progress_barrier_transition(&event.state, expected, next) {
            return Err(invalid(
                "ChangeBatch progress does not match the workspace transition",
            ));
        }
        let bytes = canonical_bytes(event)?;
        validate_progress_contract(event, &bytes)?;
        let batch_id = &event.identity.batch_id.0;
        let authority = load_authority(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch progress has no retained intent"))?;
        if event.identity != authority {
            return Err(conflict("ChangeBatch progress authority changed"));
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch workspace progress transaction cannot begin"))?;
        let barrier = load_workspace_barrier_row(&transaction, workspace_id)?
            .map(|row| workspace_barrier_from_row(workspace_id, row))
            .transpose()?
            .ok_or_else(|| conflict("ChangeBatch workspace barrier does not exist"))?;
        let existing = transaction
            .query_row(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 AND sequence = ?2",
                params![batch_id, event.sequence],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch workspace progress cannot be read"))?;
        if let Some(existing) = existing {
            if existing == bytes
                && barrier.active_batch_id.as_ref() == Some(&event.identity.batch_id)
                && barrier.state == next
            {
                transaction.commit().map_err(|_| {
                    unavailable("ChangeBatch workspace progress replay cannot commit")
                })?;
                return Ok(JournalRetention::Replay);
            }
            return Err(conflict("ChangeBatch workspace progress changed on replay"));
        }
        if barrier.active_batch_id.as_ref() != Some(&event.identity.batch_id)
            || barrier.state != expected
        {
            return Err(conflict("ChangeBatch workspace progress state is stale"));
        }
        validate_next_progress(&transaction, event)?;
        let changed = transaction
            .execute(
                "UPDATE change_batch_workspace_barrier SET state = ?4, updated_at = ?5
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = ?3",
                params![
                    workspace_id,
                    batch_id,
                    expected.as_str(),
                    next.as_str(),
                    event.occurred_at.0,
                ],
            )
            .map_err(|_| unavailable("ChangeBatch workspace progress cannot move barrier"))?;
        if changed != 1 {
            return Err(conflict(
                "ChangeBatch workspace progress compare-and-set failed",
            ));
        }
        transaction
            .execute(
                "INSERT INTO change_batch_progress (batch_id, sequence, event_json, acknowledged)
                 VALUES (?1, ?2, ?3, 0)",
                params![batch_id, event.sequence, bytes],
            )
            .map_err(|_| unavailable("ChangeBatch workspace progress cannot be persisted"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch workspace progress cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Retains the exact checkpoint identity after mutation and before validation.
    ///
    /// # Errors
    ///
    /// Rejects changed checkpoint bytes, foreign batches, or stale state.
    pub fn retain_workspace_checkpoint(
        &mut self,
        workspace_id: &str,
        batch_id: &ChangeBatchId,
        result_revision: &WorkspaceRevision,
        delta_digest: &Sha256Digest,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        if !valid_workspace_revision(result_revision)
            || !valid_digest(delta_digest)
            || now.0.is_empty()
        {
            return Err(invalid("ChangeBatch workspace checkpoint is invalid"));
        }
        let barrier = self
            .workspace_barrier(workspace_id)?
            .ok_or_else(|| conflict("ChangeBatch workspace barrier does not exist"))?;
        if barrier.active_batch_id.as_ref() != Some(batch_id) {
            return Err(conflict("ChangeBatch checkpoint batch is foreign"));
        }
        if barrier.state == ActiveBatchState::Checkpointed
            && barrier.checkpoint_revision.as_ref() == Some(result_revision)
            && barrier.checkpoint_delta_digest.as_ref() == Some(delta_digest)
        {
            return Ok(JournalRetention::Replay);
        }
        if barrier.state != ActiveBatchState::CheckpointPending {
            return Err(conflict("ChangeBatch checkpoint state is stale"));
        }
        let changed = self
            .connection
            .execute(
                "UPDATE change_batch_workspace_barrier
                 SET state = ?4, checkpoint_revision = ?5,
                     checkpoint_delta_digest = ?6, updated_at = ?7
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = ?3",
                params![
                    workspace_id,
                    batch_id.0,
                    ActiveBatchState::CheckpointPending.as_str(),
                    ActiveBatchState::Checkpointed.as_str(),
                    result_revision.0,
                    delta_digest.0,
                    now.0,
                ],
            )
            .map_err(|_| unavailable("ChangeBatch workspace checkpoint cannot be retained"))?;
        if changed != 1 {
            return Err(conflict("ChangeBatch checkpoint compare-and-set failed"));
        }
        Ok(JournalRetention::Inserted)
    }

    /// Atomically checkpoints an applied tree, appends `Applied`, and emits its receipt.
    ///
    /// # Errors
    ///
    /// Rejects a stale barrier, changed replay, invalid progress, or a receipt
    /// that does not bind the exact checkpoint revision and delta.
    #[allow(clippy::too_many_lines)]
    pub fn retain_applied_checkpoint(
        &mut self,
        workspace_id: &str,
        progress: &ChangeBatchProgressEvent,
        receipt: &ChangeBatchReceipt,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        let result_revision = receipt
            .result_revision
            .as_ref()
            .ok_or_else(|| invalid("Applied ChangeBatch receipt has no result revision"))?;
        let delta_digest = receipt
            .delta_digest
            .as_ref()
            .ok_or_else(|| invalid("Applied ChangeBatch receipt has no delta digest"))?;
        let progress_bytes = canonical_bytes(progress)?;
        validate_progress_contract(progress, &progress_bytes)?;
        let receipt_bytes = canonical_bytes(receipt)?;
        let batch_id = &progress.identity.batch_id;
        let authority = load_authority(&self.connection, &batch_id.0)?
            .ok_or_else(|| conflict("Applied checkpoint has no retained intent"))?;
        let base_revision = load_base_revision(&self.connection, &batch_id.0)?
            .ok_or_else(|| conflict("Applied checkpoint has no sealed base"))?;
        validate_receipt(progress, receipt, &authority, &base_revision)?;
        if progress.state != ChangeBatchProgressState::Applied
            || receipt.status != ChangeBatchReceiptStatus::Applied
            || !valid_workspace_revision(result_revision)
            || !valid_digest(delta_digest)
        {
            return Err(invalid("Applied ChangeBatch checkpoint is invalid"));
        }

        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("Applied checkpoint transaction cannot begin"))?;
        let barrier = load_workspace_barrier_row(&transaction, workspace_id)?
            .map(|row| workspace_barrier_from_row(workspace_id, row))
            .transpose()?
            .ok_or_else(|| conflict("Applied checkpoint workspace barrier is missing"))?;
        let existing_progress = transaction
            .query_row(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 AND sequence = ?2",
                params![batch_id.0, progress.sequence],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("Applied checkpoint progress cannot be read"))?;
        let existing_receipt = transaction
            .query_row(
                "SELECT receipt_json FROM change_batch_receipt_outbox WHERE batch_id = ?1",
                params![batch_id.0],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("Applied checkpoint receipt cannot be read"))?;
        if barrier.state == ActiveBatchState::Checkpointed
            && barrier.active_batch_id.as_ref() == Some(batch_id)
            && barrier.checkpoint_revision.as_ref() == Some(result_revision)
            && barrier.checkpoint_delta_digest.as_ref() == Some(delta_digest)
            && existing_progress.as_deref() == Some(progress_bytes.as_slice())
            && existing_receipt.as_deref() == Some(receipt_bytes.as_slice())
        {
            transaction
                .commit()
                .map_err(|_| unavailable("Applied checkpoint replay cannot commit"))?;
            return Ok(JournalRetention::Replay);
        }
        if barrier.state != ActiveBatchState::CheckpointPending
            || barrier.active_batch_id.as_ref() != Some(batch_id)
            || existing_progress.is_some()
            || existing_receipt.is_some()
        {
            return Err(conflict("Applied checkpoint compare-and-set failed"));
        }
        let events = load_progress_events(&transaction, &batch_id.0)?;
        let mut ledger = ChangeBatchProgressLedger::new();
        for event in &events {
            ledger
                .record(event)
                .map_err(|_| corrupt("Stored ChangeBatch progress order is invalid"))?;
        }
        ledger
            .record(progress)
            .map_err(|_| conflict("Applied checkpoint progress is invalid"))?;
        let barrier_changed = transaction
            .execute(
                "UPDATE change_batch_workspace_barrier
                 SET state = ?4, checkpoint_revision = ?5,
                     checkpoint_delta_digest = ?6, updated_at = ?7
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = ?3",
                params![
                    workspace_id,
                    batch_id.0,
                    ActiveBatchState::CheckpointPending.as_str(),
                    ActiveBatchState::Checkpointed.as_str(),
                    result_revision.0,
                    delta_digest.0,
                    now.0,
                ],
            )
            .map_err(|_| unavailable("Applied workspace checkpoint cannot be persisted"))?;
        let execution_changed = transaction
            .execute(
                "UPDATE change_batch_execution
                 SET receipt_json = ?2, phase = ?3, updated_at = ?4
                 WHERE batch_id = ?1 AND receipt_json IS NULL",
                params![
                    batch_id.0,
                    receipt_bytes,
                    ChangeBatchExecutionPhase::Applied.as_str(),
                    now.0,
                ],
            )
            .map_err(|_| unavailable("Applied checkpoint receipt cannot be persisted"))?;
        if barrier_changed != 1 || execution_changed != 1 {
            return Err(conflict("Applied checkpoint compare-and-set failed"));
        }
        transaction
            .execute(
                "INSERT INTO change_batch_progress (batch_id, sequence, event_json, acknowledged)
                 VALUES (?1, ?2, ?3, 0)",
                params![batch_id.0, progress.sequence, progress_bytes],
            )
            .map_err(|_| unavailable("Applied checkpoint progress cannot be persisted"))?;
        transaction
            .execute(
                "INSERT INTO change_batch_receipt_outbox (batch_id, receipt_json, acknowledged)
                 VALUES (?1, ?2, 0)",
                params![batch_id.0, receipt_bytes],
            )
            .map_err(|_| unavailable("Applied checkpoint receipt outbox cannot be persisted"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("Applied checkpoint transaction cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Accepts exactly the observed checkpoint and releases the single-Writer barrier.
    ///
    /// Stale or foreign observation facts return `Stale` without changing state.
    ///
    /// # Errors
    ///
    /// Rejects unavailable or corrupt durable state.
    pub fn accept_observed_checkpoint(
        &mut self,
        workspace_id: &str,
        progress: &ChangeBatchProgressEvent,
        observed_revision: &WorkspaceRevision,
        observed_delta_digest: &Sha256Digest,
        now: &Instant,
    ) -> Result<ObservationGateResult, ChangeBatchJournalError> {
        let progress_bytes = canonical_bytes(progress)?;
        validate_progress_contract(progress, &progress_bytes)?;
        if progress.state != ChangeBatchProgressState::Accepted {
            return Err(invalid("ChangeBatch acceptance progress is invalid"));
        }
        let batch_id = &progress.identity.batch_id;
        let authority = load_authority(&self.connection, &batch_id.0)?
            .ok_or_else(|| conflict("ChangeBatch acceptance has no retained intent"))?;
        if progress.identity != authority {
            return Ok(ObservationGateResult::Stale);
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch acceptance transaction cannot begin"))?;
        let Some(barrier) = load_workspace_barrier_row(&transaction, workspace_id)?
            .map(|row| workspace_barrier_from_row(workspace_id, row))
            .transpose()?
        else {
            return Ok(ObservationGateResult::Stale);
        };
        let existing_progress = transaction
            .query_row(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 AND sequence = ?2",
                params![batch_id.0, progress.sequence],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch acceptance progress cannot be read"))?;
        if barrier.state == ActiveBatchState::Accepted
            && barrier.active_batch_id.is_none()
            && barrier.accepted_revision == *observed_revision
            && existing_progress.as_deref() == Some(progress_bytes.as_slice())
            && receipt_delta_matches(&transaction, batch_id, observed_delta_digest)?
        {
            transaction
                .commit()
                .map_err(|_| unavailable("ChangeBatch acceptance replay cannot commit"))?;
            return Ok(ObservationGateResult::Accepted);
        }
        if barrier.state != ActiveBatchState::ObservationPending
            || barrier.active_batch_id.as_ref() != Some(batch_id)
            || barrier.checkpoint_revision.as_ref() != Some(observed_revision)
            || barrier.checkpoint_delta_digest.as_ref() != Some(observed_delta_digest)
            || existing_progress.is_some()
        {
            return Ok(ObservationGateResult::Stale);
        }
        validate_next_progress(&transaction, progress)?;
        let changed = transaction
            .execute(
                "UPDATE change_batch_workspace_barrier
                 SET accepted_revision = checkpoint_revision, active_batch_id = NULL,
                     state = ?4, checkpoint_revision = NULL,
                     checkpoint_delta_digest = NULL, updated_at = ?5
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = ?3
                   AND checkpoint_revision = ?6 AND checkpoint_delta_digest = ?7",
                params![
                    workspace_id,
                    batch_id.0,
                    ActiveBatchState::ObservationPending.as_str(),
                    ActiveBatchState::Accepted.as_str(),
                    now.0,
                    observed_revision.0,
                    observed_delta_digest.0,
                ],
            )
            .map_err(|_| unavailable("ChangeBatch workspace acceptance cannot be persisted"))?;
        if changed != 1 {
            return Ok(ObservationGateResult::Stale);
        }
        transaction
            .execute(
                "INSERT INTO change_batch_progress (batch_id, sequence, event_json, acknowledged)
                 VALUES (?1, ?2, ?3, 0)",
                params![batch_id.0, progress.sequence, progress_bytes],
            )
            .map_err(|_| unavailable("ChangeBatch acceptance progress cannot be persisted"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch acceptance cannot commit"))?;
        Ok(ObservationGateResult::Accepted)
    }

    /// Retains the exact executable selection or non-executable advisory decision.
    ///
    /// # Errors
    ///
    /// Rejects malformed selections, foreign batches, stale first writes, and
    /// same-batch changed selection bytes. Exact durable replay remains valid
    /// after the barrier advances.
    pub fn retain_phase_selection(
        &mut self,
        workspace_id: &str,
        batch_id: &ChangeBatchId,
        selection: &ValidationProfileSelection,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        let bytes = canonical_bytes(selection)?;
        let decoded: ValidationProfileSelection = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("ChangeBatch validation selection is non-canonical"))?;
        if decoded != *selection || !valid_phase_selection(selection) || now.0.is_empty() {
            return Err(invalid("ChangeBatch validation selection is invalid"));
        }
        let digest = digest_bytes(&bytes);
        let existing = self
            .connection
            .query_row(
                "SELECT selection_json, selection_digest FROM change_batch_phase_run
                 WHERE batch_id = ?1",
                params![batch_id.0],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch validation selection cannot be read"))?;
        if let Some((existing_bytes, existing_digest)) = existing {
            if existing_bytes == bytes && existing_digest == digest.0 {
                return Ok(JournalRetention::Replay);
            }
            return Err(conflict(
                "ChangeBatch validation selection changed on replay",
            ));
        }
        let barrier = self
            .workspace_barrier(workspace_id)?
            .ok_or_else(|| conflict("ChangeBatch validation barrier is missing"))?;
        if barrier.active_batch_id.as_ref() != Some(batch_id)
            || barrier.state != ActiveBatchState::Applying
            || load_authority(&self.connection, &batch_id.0)?.is_none()
        {
            return Err(conflict(
                "ChangeBatch validation selection authority is stale",
            ));
        }
        self.connection
            .execute(
                "INSERT INTO change_batch_phase_run
                 (batch_id, selection_json, selection_digest, next_command,
                  normalizer_receipt_json, validation_receipt_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 0, NULL, NULL, ?4, ?4)",
                params![batch_id.0, bytes, digest.0, now.0],
            )
            .map_err(|_| unavailable("ChangeBatch validation selection cannot be persisted"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Retains one bounded command result and advances the exact selection cursor.
    ///
    /// # Errors
    ///
    /// Rejects gaps, changed replay, a foreign command id, or a missing selection.
    pub fn retain_phase_command_receipt(
        &mut self,
        batch_id: &ChangeBatchId,
        ordinal: usize,
        receipt: &PhaseProcessReceipt,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        self.retain_phase_command_result(batch_id, ordinal, receipt, None, false, now)
    }

    /// Atomically retains one command receipt and its explicitly versioned diagnostic snapshot.
    ///
    /// # Errors
    ///
    /// Rejects gaps, changed replay, a foreign command id, or malformed diagnostics.
    pub fn retain_phase_command_result(
        &mut self,
        batch_id: &ChangeBatchId,
        ordinal: usize,
        receipt: &PhaseProcessReceipt,
        diagnostics: Option<&DiagnosticParseBatch>,
        diagnostic_parse_failed: bool,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        let record = load_phase_record(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch validation selection is missing"))?;
        if let Some(existing) = record.command_receipts.get(ordinal) {
            return if existing == receipt
                && record
                    .diagnostic_batches
                    .get(ordinal)
                    .and_then(Option::as_ref)
                    == diagnostics
                && record.diagnostic_parse_failures.get(ordinal).copied()
                    == Some(diagnostic_parse_failed)
            {
                Ok(JournalRetention::Replay)
            } else {
                Err(conflict("ChangeBatch validation command changed on replay"))
            };
        }
        if record.command_receipts.len() != ordinal
            || record.selection.command_ids.get(ordinal) != Some(&receipt.name)
            || now.0.is_empty()
        {
            return Err(conflict("ChangeBatch validation command cursor is stale"));
        }
        let bytes = canonical_bytes(receipt)?;
        let diagnostic_bytes = diagnostics
            .map(PersistedDiagnosticParseBatch::from)
            .map(|batch| canonical_bytes(&batch))
            .transpose()?;
        let ordinal = i64::try_from(ordinal)
            .map_err(|_| capacity("ChangeBatch validation command ordinal is too large"))?;
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch validation command transaction cannot begin"))?;
        let changed = transaction
            .execute(
                "UPDATE change_batch_phase_run SET next_command = ?3, updated_at = ?4
                 WHERE batch_id = ?1 AND next_command = ?2",
                params![batch_id.0, ordinal, ordinal + 1, now.0],
            )
            .map_err(|_| unavailable("ChangeBatch validation cursor cannot advance"))?;
        if changed != 1 {
            return Err(conflict(
                "ChangeBatch validation command compare-and-set failed",
            ));
        }
        transaction
            .execute(
                "INSERT INTO change_batch_phase_command
                 (batch_id, ordinal, command_id, receipt_json, diagnostic_batch_json,
                  diagnostic_parse_failed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    batch_id.0,
                    ordinal,
                    receipt.name,
                    bytes,
                    diagnostic_bytes,
                    i64::from(diagnostic_parse_failed)
                ],
            )
            .map_err(|_| unavailable("ChangeBatch validation command cannot be persisted"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch validation command cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Returns the exact persisted Writer/Validation execution state.
    ///
    /// # Errors
    ///
    /// Rejects changed digests, invalid cursor order, and corrupt receipts.
    pub fn phase_record(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Option<ChangeBatchPhaseRecord>, ChangeBatchJournalError> {
        load_phase_record(&self.connection, batch_id)
    }

    /// Loads the exact comparable diagnostic baseline for this batch's selected profile.
    ///
    /// # Errors
    ///
    /// Rejects missing selection authority or altered baseline bytes.
    pub fn diagnostic_baseline(
        &self,
        batch_id: &ChangeBatchId,
        revision: &WorkspaceRevision,
    ) -> Result<Option<DiagnosticBaseline>, ChangeBatchJournalError> {
        let record = load_phase_record(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch validation selection is missing"))?;
        let scope = diagnostic_scope_digest(&record.selection)?;
        self.connection
            .query_row(
                "SELECT baseline_json, baseline_digest
                 FROM change_batch_diagnostic_baseline
                 WHERE scope_digest = ?1 AND workspace_revision = ?2",
                params![scope.0, revision.0],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch diagnostic baseline cannot be read"))?
            .map(|(bytes, digest)| decode_diagnostic_baseline(&bytes, &digest, revision))
            .transpose()
    }

    /// Loads and fully revalidates one retained deterministic diagnostic decision.
    ///
    /// # Errors
    ///
    /// Rejects changed bytes, invalid baselines/comparison, or authority revision drift.
    pub fn diagnostic_evaluation(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Option<ValidationDiagnosticEvaluation>, ChangeBatchJournalError> {
        let Some((bytes, digest)) = self
            .connection
            .query_row(
                "SELECT evaluation_json, evaluation_digest
                 FROM change_batch_diagnostic_evaluation WHERE batch_id = ?1",
                params![batch_id.0],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch diagnostic evaluation cannot be read"))?
        else {
            return Ok(None);
        };
        if digest_bytes(&bytes).0 != digest {
            return Err(corrupt("ChangeBatch diagnostic evaluation digest changed"));
        }
        let evaluation: ValidationDiagnosticEvaluation = serde_json::from_slice(&bytes)
            .map_err(|_| corrupt("ChangeBatch diagnostic evaluation is corrupt"))?;
        if canonical_bytes(&evaluation)? != bytes
            || validate_diagnostic_evaluation(&evaluation).is_err()
        {
            return Err(corrupt("ChangeBatch diagnostic evaluation is invalid"));
        }
        let execution = self
            .load(batch_id)?
            .ok_or_else(|| corrupt("ChangeBatch diagnostic authority is missing"))?;
        let phase = load_phase_record(&self.connection, batch_id)?
            .ok_or_else(|| corrupt("ChangeBatch diagnostic selection is missing"))?;
        let Some(validation_receipt) = phase.validation_receipt.as_ref() else {
            return Err(corrupt("ChangeBatch diagnostic validation is missing"));
        };
        if execution.base_revision != evaluation.base_revision
            || validation_receipt.result_revision.as_ref() != Some(&evaluation.result_revision)
        {
            return Err(corrupt("ChangeBatch diagnostic authority changed"));
        }
        validate_retained_diagnostic_decision(&evaluation, &validation_receipt.status)
            .map_err(|_| corrupt("ChangeBatch diagnostic decision changed"))?;
        Ok(Some(evaluation))
    }

    /// Atomically retains one deterministic evaluation and its result baseline.
    ///
    /// # Errors
    ///
    /// Rejects revision drift, changed replay, invalid comparisons, and stale authority.
    pub fn retain_diagnostic_evaluation(
        &mut self,
        batch_id: &ChangeBatchId,
        evaluation: &ValidationDiagnosticEvaluation,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        validate_diagnostic_evaluation(evaluation)?;
        let execution = self
            .load(batch_id)?
            .ok_or_else(|| conflict("ChangeBatch diagnostic authority is missing"))?;
        if execution.base_revision != evaluation.base_revision {
            return Err(conflict("ChangeBatch diagnostic base revision is stale"));
        }
        let record = load_phase_record(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch validation selection is missing"))?;
        let Some(validation_receipt) = record.validation_receipt.as_ref() else {
            return Err(conflict("ChangeBatch diagnostic validation is missing"));
        };
        if validation_receipt.result_revision.as_ref() != Some(&evaluation.result_revision)
            || now.0.is_empty()
        {
            return Err(conflict(
                "ChangeBatch diagnostic evaluation revision is stale",
            ));
        }
        validate_retained_diagnostic_decision(evaluation, &validation_receipt.status)?;
        let scope = diagnostic_scope_digest(&record.selection)?;
        let bytes = canonical_bytes(evaluation)?;
        let digest = digest_bytes(&bytes);
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch diagnostic evaluation cannot begin"))?;
        let existing = transaction
            .query_row(
                "SELECT evaluation_json, evaluation_digest
                 FROM change_batch_diagnostic_evaluation WHERE batch_id = ?1",
                params![batch_id.0],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch diagnostic evaluation cannot be read"))?;
        if let Some((existing_bytes, existing_digest)) = existing {
            if existing_bytes == bytes && existing_digest == digest.0 {
                return Ok(JournalRetention::Replay);
            }
            return Err(conflict(
                "ChangeBatch diagnostic evaluation changed on replay",
            ));
        }
        if let Some(result) = evaluation.result.as_ref() {
            let baseline_bytes = canonical_bytes(result)?;
            let baseline_digest = digest_bytes(&baseline_bytes);
            let changed = transaction
                .execute(
                    "INSERT INTO change_batch_diagnostic_baseline
                     (scope_digest, workspace_revision, baseline_json, baseline_digest)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(scope_digest, workspace_revision) DO UPDATE SET
                       baseline_json = excluded.baseline_json,
                       baseline_digest = excluded.baseline_digest
                     WHERE baseline_json = excluded.baseline_json
                       AND baseline_digest = excluded.baseline_digest",
                    params![
                        scope.0,
                        result.workspace_revision.0,
                        baseline_bytes,
                        baseline_digest.0
                    ],
                )
                .map_err(|_| conflict("ChangeBatch result diagnostic baseline changed"))?;
            if changed != 1 {
                return Err(conflict("ChangeBatch result diagnostic baseline changed"));
            }
        }
        transaction
            .execute(
                "INSERT INTO change_batch_diagnostic_evaluation
                 (batch_id, evaluation_json, evaluation_digest, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![batch_id.0, bytes, digest.0, now.0],
            )
            .map_err(|_| unavailable("ChangeBatch diagnostic evaluation cannot persist"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch diagnostic evaluation cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Retains the revision-bound normalizer receipt before checkpointing.
    ///
    /// # Errors
    ///
    /// Rejects changed replay, incomplete command execution, or revision drift.
    pub fn retain_normalizer_receipt(
        &mut self,
        batch_id: &ChangeBatchId,
        receipt: &NormalizerReceipt,
        completed_commands: usize,
        expected_base: &WorkspaceRevision,
        expected_result: Option<&WorkspaceRevision>,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        validate_normalizer_receipt_binding(receipt, expected_base, expected_result)
            .map_err(|_| invalid("ChangeBatch normalizer receipt revision is invalid"))?;
        let record = load_phase_record(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch validation selection is missing"))?;
        if record.command_receipts.len() != completed_commands
            || completed_commands > record.selection.command_ids.len()
        {
            return Err(conflict(
                "ChangeBatch normalizer command cursor is incomplete",
            ));
        }
        retain_phase_receipt(
            &self.connection,
            batch_id,
            "normalizer_receipt_json",
            receipt,
            now,
        )
    }

    /// Retains the exact read-only validation receipt.
    ///
    /// # Errors
    ///
    /// Rejects changed replay, incomplete command execution, or revision drift.
    pub fn retain_validation_receipt(
        &mut self,
        batch_id: &ChangeBatchId,
        receipt: &ValidationReceipt,
        completed_commands: usize,
        expected_base: &WorkspaceRevision,
        expected_result: Option<&WorkspaceRevision>,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        validate_validation_receipt_binding(receipt, expected_base, expected_result)
            .map_err(|_| invalid("ChangeBatch validation receipt revision is invalid"))?;
        let record = load_phase_record(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch validation selection is missing"))?;
        if record.command_receipts.len() != completed_commands
            || completed_commands > record.selection.command_ids.len()
        {
            return Err(conflict(
                "ChangeBatch validation command cursor is incomplete",
            ));
        }
        retain_phase_receipt(
            &self.connection,
            batch_id,
            "validation_receipt_json",
            receipt,
            now,
        )
    }

    /// Atomically appends `validation_completed` and upgrades the emitted checkpoint receipt.
    ///
    /// # Errors
    ///
    /// Rejects revision drift, a changed pre-validation receipt, a stale barrier,
    /// or a non-contiguous progress stream.
    pub fn retain_validated_checkpoint(
        &mut self,
        workspace_id: &str,
        progress: &ChangeBatchProgressEvent,
        receipt: &ChangeBatchReceipt,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        if progress.state != ChangeBatchProgressState::ValidationCompleted
            || receipt.status != ChangeBatchReceiptStatus::Applied
            || receipt.validation.is_none()
        {
            return Err(invalid("Validated ChangeBatch checkpoint is invalid"));
        }
        let progress_bytes = canonical_bytes(progress)?;
        validate_progress_contract(progress, &progress_bytes)?;
        let receipt_bytes = canonical_bytes(receipt)?;
        let batch_id = &progress.identity.batch_id;
        let authority = load_authority(&self.connection, &batch_id.0)?
            .ok_or_else(|| conflict("Validated checkpoint has no retained intent"))?;
        let base_revision = load_base_revision(&self.connection, &batch_id.0)?
            .ok_or_else(|| conflict("Validated checkpoint has no sealed base"))?;
        validate_receipt_without_progress(receipt, &authority, &base_revision)?;
        if progress.identity != authority || receipt.identity != authority {
            return Err(invalid("Validated checkpoint authority changed"));
        }
        let mut prior_receipt = receipt.clone();
        prior_receipt.validation = None;
        let prior_bytes = canonical_bytes(&prior_receipt)?;
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("Validated checkpoint transaction cannot begin"))?;
        let barrier = load_workspace_barrier_row(&transaction, workspace_id)?
            .map(|row| workspace_barrier_from_row(workspace_id, row))
            .transpose()?
            .ok_or_else(|| conflict("Validated checkpoint barrier is missing"))?;
        let existing_progress = transaction
            .query_row(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 AND sequence = ?2",
                params![batch_id.0, progress.sequence],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("Validated checkpoint progress cannot be read"))?;
        let existing_receipt = transaction
            .query_row(
                "SELECT receipt_json FROM change_batch_receipt_outbox WHERE batch_id = ?1",
                params![batch_id.0],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("Validated checkpoint receipt cannot be read"))?;
        if existing_progress.as_deref() == Some(progress_bytes.as_slice())
            && existing_receipt.as_deref() == Some(receipt_bytes.as_slice())
            && barrier.state == ActiveBatchState::ValidationPending
        {
            transaction
                .commit()
                .map_err(|_| unavailable("Validated checkpoint replay cannot commit"))?;
            return Ok(JournalRetention::Replay);
        }
        if barrier.state != ActiveBatchState::ValidationPending
            || barrier.active_batch_id.as_ref() != Some(batch_id)
            || barrier.checkpoint_revision.as_ref() != receipt.result_revision.as_ref()
            || existing_progress.is_some()
            || existing_receipt.as_deref() != Some(prior_bytes.as_slice())
        {
            return Err(conflict("Validated checkpoint compare-and-set failed"));
        }
        validate_next_progress(&transaction, progress)?;
        let execution_changed = transaction
            .execute(
                "UPDATE change_batch_execution SET receipt_json = ?3, updated_at = ?4
                 WHERE batch_id = ?1 AND receipt_json = ?2",
                params![batch_id.0, prior_bytes, receipt_bytes, now.0],
            )
            .map_err(|_| unavailable("Validated execution receipt cannot be upgraded"))?;
        let outbox_changed = transaction
            .execute(
                "UPDATE change_batch_receipt_outbox SET receipt_json = ?3
                 WHERE batch_id = ?1 AND receipt_json = ?2 AND acknowledged = 0",
                params![batch_id.0, prior_bytes, receipt_bytes],
            )
            .map_err(|_| unavailable("Validated receipt outbox cannot be upgraded"))?;
        if execution_changed != 1 || outbox_changed != 1 {
            return Err(conflict("Validated checkpoint receipt is already consumed"));
        }
        transaction
            .execute(
                "INSERT INTO change_batch_progress (batch_id, sequence, event_json, acknowledged)
                 VALUES (?1, ?2, ?3, 0)",
                params![batch_id.0, progress.sequence, progress_bytes],
            )
            .map_err(|_| unavailable("Validated checkpoint progress cannot be persisted"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("Validated checkpoint cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Opens the private journal schema used by all runtime operations.
    fn initialize_schema_v4(connection: &Connection) -> Result<(), ChangeBatchJournalError> {
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS change_batch_execution (
                   batch_id TEXT PRIMARY KEY NOT NULL,
                   proposal_event_json BLOB NOT NULL,
                   intent_digest TEXT NOT NULL,
                   proposal_digest TEXT NOT NULL,
                   authority_json BLOB NOT NULL,
                   authority_digest TEXT NOT NULL,
                   base_revision TEXT NOT NULL,
                   plan_digest TEXT NOT NULL,
                   phase TEXT NOT NULL,
                   next_operation INTEGER NOT NULL,
                   receipt_json BLOB,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_progress (
                   batch_id TEXT NOT NULL,
                   sequence INTEGER NOT NULL,
                   event_json BLOB NOT NULL,
                   acknowledged INTEGER NOT NULL DEFAULT 0,
                   PRIMARY KEY (batch_id, sequence),
                   FOREIGN KEY (batch_id) REFERENCES change_batch_execution(batch_id)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_rollback_entry (
                   batch_id TEXT NOT NULL,
                   ordinal INTEGER NOT NULL,
                   entry_json BLOB NOT NULL,
                   PRIMARY KEY (batch_id, ordinal),
                   FOREIGN KEY (batch_id) REFERENCES change_batch_execution(batch_id)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_receipt_outbox (
                   batch_id TEXT PRIMARY KEY NOT NULL,
                   receipt_json BLOB NOT NULL,
                   acknowledged INTEGER NOT NULL DEFAULT 0,
                   FOREIGN KEY (batch_id) REFERENCES change_batch_execution(batch_id)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_mutation_preimage_manifest (
                   batch_id TEXT PRIMARY KEY NOT NULL,
                   manifest_json BLOB NOT NULL,
                   FOREIGN KEY (batch_id) REFERENCES change_batch_execution(batch_id)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_workspace_barrier (
                   workspace_id TEXT PRIMARY KEY NOT NULL,
                   accepted_revision TEXT NOT NULL,
                   active_batch_id TEXT UNIQUE,
                   state TEXT NOT NULL,
                   checkpoint_revision TEXT,
                   checkpoint_delta_digest TEXT,
                   updated_at TEXT NOT NULL,
                   FOREIGN KEY (active_batch_id) REFERENCES change_batch_execution(batch_id)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_workspace_restore_intent (
                   workspace_id TEXT NOT NULL,
                   expected_current TEXT NOT NULL,
                   target_revision TEXT NOT NULL,
                   PRIMARY KEY (workspace_id, expected_current, target_revision)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_phase_run (
                   batch_id TEXT PRIMARY KEY NOT NULL,
                   selection_json BLOB NOT NULL,
                   selection_digest TEXT NOT NULL,
                   next_command INTEGER NOT NULL,
                   normalizer_receipt_json BLOB,
                   validation_receipt_json BLOB,
                   created_at TEXT NOT NULL,
                   updated_at TEXT NOT NULL,
                   FOREIGN KEY (batch_id) REFERENCES change_batch_execution(batch_id)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_phase_command (
                   batch_id TEXT NOT NULL,
                   ordinal INTEGER NOT NULL,
                   command_id TEXT NOT NULL,
                   receipt_json BLOB NOT NULL,
                   diagnostic_batch_json BLOB,
                   diagnostic_parse_failed INTEGER NOT NULL DEFAULT 0,
                   PRIMARY KEY (batch_id, ordinal),
                   FOREIGN KEY (batch_id) REFERENCES change_batch_execution(batch_id)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_diagnostic_baseline (
                   scope_digest TEXT NOT NULL,
                   workspace_revision TEXT NOT NULL,
                   baseline_json BLOB NOT NULL,
                   baseline_digest TEXT NOT NULL,
                   PRIMARY KEY (scope_digest, workspace_revision)
                 );
                 CREATE TABLE IF NOT EXISTS change_batch_diagnostic_evaluation (
                   batch_id TEXT PRIMARY KEY NOT NULL,
                   evaluation_json BLOB NOT NULL,
                   evaluation_digest TEXT NOT NULL,
                   created_at TEXT NOT NULL,
                   FOREIGN KEY (batch_id) REFERENCES change_batch_execution(batch_id)
                 );",
            )
            .map_err(|_| unavailable("ChangeBatch journal schema cannot be initialized"))
    }

    /// Retains one exact proposal and its deterministic plan identity.
    ///
    /// # Errors
    ///
    /// Rejects invalid identity or patch digests and same-key changed bytes.
    pub fn retain_intent(
        &mut self,
        event: &ChangeBatchProposalEvent,
        resolved_source_commit: &WorkspaceRevision,
        plan_digest: &Sha256Digest,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        retain_intent_on(
            &self.connection,
            event,
            resolved_source_commit,
            plan_digest,
            now,
        )
    }

    /// Atomically retains one intent and claims the workspace's Writer barrier.
    ///
    /// # Errors
    ///
    /// Rejects changed replay, stale accepted revision, or another active batch.
    pub fn retain_claimed_intent(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProposalEvent,
        expected_base_revision: &WorkspaceRevision,
        plan_digest: &Sha256Digest,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch claimed intent transaction cannot begin"))?;
        let retention = retain_intent_on(
            &transaction,
            event,
            expected_base_revision,
            plan_digest,
            now,
        )?;
        let row = load_workspace_barrier_row(&transaction, workspace_id)?
            .ok_or_else(|| conflict("ChangeBatch workspace barrier does not exist"))?;
        let barrier = workspace_barrier_from_row(workspace_id, row)?;
        if barrier.accepted_revision != *expected_base_revision {
            return Err(conflict("ChangeBatch workspace base revision is stale"));
        }
        if barrier.active_batch_id.as_ref() == Some(&event.identity.batch_id) {
            transaction
                .commit()
                .map_err(|_| unavailable("ChangeBatch claimed intent cannot commit"))?;
            return Ok(JournalRetention::Replay);
        }
        if barrier.active_batch_id.is_some()
            || !matches!(
                barrier.state,
                ActiveBatchState::Idle | ActiveBatchState::Accepted
            )
        {
            return Err(conflict(
                "ChangeBatch workspace already has an active batch",
            ));
        }
        let changed = transaction
            .execute(
                "UPDATE change_batch_workspace_barrier
                 SET active_batch_id = ?2, state = ?3,
                     checkpoint_revision = NULL, checkpoint_delta_digest = NULL,
                     updated_at = ?4
                 WHERE workspace_id = ?1 AND active_batch_id IS NULL
                   AND accepted_revision = ?5 AND state IN ('idle', 'accepted')",
                params![
                    workspace_id,
                    event.identity.batch_id.0,
                    ActiveBatchState::Applying.as_str(),
                    now.0,
                    expected_base_revision.0,
                ],
            )
            .map_err(|_| unavailable("ChangeBatch workspace barrier cannot be claimed"))?;
        if changed != 1 {
            return Err(conflict(
                "ChangeBatch workspace barrier compare-and-set failed",
            ));
        }
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch claimed intent cannot commit"))?;
        Ok(retention)
    }

    /// Returns one fully revalidated execution record.
    ///
    /// # Errors
    ///
    /// Rejects missing, corrupt, or independently altered durable facts.
    pub fn load(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Option<ChangeBatchJournalRecord>, ChangeBatchJournalError> {
        let record = load_execution_row(&self.connection, &batch_id.0)?
            .map(|row| execution_record(&row))
            .transpose()?;
        if let Some(record) = record.as_ref() {
            load_mutation_preimage_record(&self.connection, &self.blob_root, record)?;
        }
        Ok(record)
    }

    /// Rebuilds the canonical mutation record from the private durable manifest.
    ///
    /// # Errors
    ///
    /// Rejects missing or altered blobs, plan drift, or any digest mismatch.
    pub fn mutation_preimage_record(
        &self,
        plan: &PreparedChangeBatchPlan,
    ) -> Result<Option<PreparedPreimageJournalRecord>, ChangeBatchJournalError> {
        let execution = load_execution_row(&self.connection, &plan.event().identity.batch_id.0)?
            .map(|row| execution_record(&row))
            .transpose()?
            .ok_or_else(|| conflict("ChangeBatch mutation has no retained intent"))?;
        if execution.event != *plan.event() || execution.plan_digest != *plan.plan_digest() {
            return Err(conflict("ChangeBatch mutation recovery plan changed"));
        }
        load_mutation_preimage_record(&self.connection, &self.blob_root, &execution)
    }

    /// Returns every revalidated durable batch record for one Job in creation order.
    ///
    /// # Errors
    ///
    /// Rejects corrupt records or an unreasonable recovery backlog.
    pub fn records_for_job(
        &self,
        job_id: &winwincode_domain::ExecutionJobId,
    ) -> Result<Vec<ChangeBatchJournalRecord>, ChangeBatchJournalError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT batch_id FROM change_batch_execution
                 ORDER BY created_at, batch_id",
            )
            .map_err(|_| unavailable("ChangeBatch recovery records cannot be opened"))?;
        let batch_ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| unavailable("ChangeBatch recovery records cannot be read"))?
            .map(|row| row.map(ChangeBatchId))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| unavailable("ChangeBatch recovery record cannot be read"))?;
        if batch_ids.len() > MAX_RECOVERY_RECORDS {
            return Err(capacity("ChangeBatch recovery backlog is too large"));
        }
        batch_ids
            .into_iter()
            .map(|batch_id| self.load(&batch_id))
            .collect::<Result<Vec<_>, _>>()
            .map(|records| {
                records
                    .into_iter()
                    .flatten()
                    .filter(|record| record.event.identity.job_id == *job_id)
                    .collect()
            })
    }

    /// Persists every preimage blob before committing its manifest and phase.
    ///
    /// # Errors
    ///
    /// Rejects invalid manifests, changed replay, non-contiguous ordinals, or
    /// more than 64 MiB before any workspace mutation is authorized.
    pub fn retain_preimages(
        &mut self,
        batch_id: &ChangeBatchId,
        preimages: &[RollbackPreimage],
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        let prepared = prepare_preimages(preimages)?;
        let total = prepared.iter().try_fold(0_u64, |sum, (_, entry)| {
            sum.checked_add(entry.before_len)
                .ok_or_else(|| capacity("ChangeBatch preimage byte count overflowed"))
        })?;
        if total > MAX_CHANGE_BATCH_PREIMAGE_BYTES {
            return Err(capacity("ChangeBatch rollback preimages exceed 64 MiB"));
        }
        let existing = load_rollback_entries(&self.connection, &batch_id.0)?;
        if !existing.is_empty() {
            let expected = prepared
                .iter()
                .map(|(_, entry)| entry.clone())
                .collect::<Vec<_>>();
            if existing == expected {
                return Ok(JournalRetention::Replay);
            }
            return Err(conflict("ChangeBatch rollback manifest changed on replay"));
        }
        let record = self
            .load(batch_id)?
            .ok_or_else(|| conflict("ChangeBatch rollback manifest has no retained intent"))?;
        if record.phase != ChangeBatchExecutionPhase::IntentRetained || now.0.is_empty() {
            return Err(conflict(
                "ChangeBatch rollback manifest is outside intent phase",
            ));
        }
        for (bytes, entry) in &prepared {
            if let (Some(bytes), Some(digest)) = (bytes, entry.blob_digest.as_ref()) {
                self.persist_blob(digest, bytes)?;
            }
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch rollback transaction cannot begin"))?;
        for (_, entry) in &prepared {
            let bytes = canonical_bytes(entry)?;
            transaction
                .execute(
                    "INSERT INTO change_batch_rollback_entry (batch_id, ordinal, entry_json)
                     VALUES (?1, ?2, ?3)",
                    params![batch_id.0, i64_from_u64(entry.ordinal)?, bytes],
                )
                .map_err(|_| unavailable("ChangeBatch rollback manifest cannot be persisted"))?;
        }
        update_phase(
            &transaction,
            &batch_id.0,
            ChangeBatchExecutionPhase::IntentRetained,
            ChangeBatchExecutionPhase::PreimagesReady,
            &now.0,
        )?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch rollback transaction cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Reads one verified private preimage blob.
    ///
    /// # Errors
    ///
    /// Rejects a missing entry, absent-file preimage, or changed blob bytes.
    pub fn read_preimage(
        &self,
        batch_id: &ChangeBatchId,
        ordinal: u64,
    ) -> Result<Vec<u8>, ChangeBatchJournalError> {
        let entry = load_rollback_entry(&self.connection, &batch_id.0, ordinal)?
            .ok_or_else(|| conflict("ChangeBatch rollback entry does not exist"))?;
        let digest = entry
            .blob_digest
            .ok_or_else(|| conflict("ChangeBatch rollback entry has no source bytes"))?;
        let bytes = fs::read(blob_path(&self.blob_root, &digest)?)
            .map_err(|_| unavailable("ChangeBatch preimage blob cannot be read"))?;
        if digest_bytes(&bytes) != digest
            || u64::try_from(bytes.len()).ok() != Some(entry.before_len)
        {
            return Err(corrupt("ChangeBatch preimage blob digest changed"));
        }
        Ok(bytes)
    }

    fn retain_mutation_preimages(
        &mut self,
        record: &PreparedPreimageJournalRecord,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        let execution = self
            .load(record.batch_id())?
            .ok_or_else(|| conflict("ChangeBatch preimages have no retained intent"))?;
        if execution.plan_digest != *record.plan_digest() {
            return Err(conflict(
                "ChangeBatch mutation preimages changed the retained plan",
            ));
        }
        if record.total_preimage_bytes() > MAX_CHANGE_BATCH_PREIMAGE_BYTES {
            return Err(capacity(
                "ChangeBatch mutation preimages exceed the retained plan",
            ));
        }
        let plan = prepare_change_batch(&execution.event, ChangeBatchPolicy::default())
            .map_err(|_| corrupt("ChangeBatch mutation recovery plan is corrupt"))?;
        validate_preimage_journal_record(&plan, record)
            .map_err(|_| invalid("ChangeBatch mutation preimage record is invalid"))?;
        let manifest = prepare_mutation_preimage_manifest(record)?;
        let manifest_bytes = canonical_bytes(&manifest)?;
        if let Some(existing) = self
            .connection
            .query_row(
                "SELECT manifest_json FROM change_batch_mutation_preimage_manifest
                 WHERE batch_id = ?1",
                params![record.batch_id().0],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch mutation preimages cannot be read"))?
        {
            return if existing == manifest_bytes {
                Ok(JournalRetention::Replay)
            } else {
                Err(conflict("ChangeBatch mutation preimages changed on replay"))
            };
        }
        if execution.phase != ChangeBatchExecutionPhase::IntentRetained {
            return Err(conflict(
                "ChangeBatch mutation preimages are outside intent phase",
            ));
        }
        for file in record.files() {
            if let (Some(bytes), Some(digest)) = (file.bytes(), file.digest()) {
                self.persist_blob(digest, bytes)?;
            }
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch mutation preimage transaction cannot begin"))?;
        transaction
            .execute(
                "INSERT INTO change_batch_mutation_preimage_manifest (batch_id, manifest_json)
                 VALUES (?1, ?2)",
                params![record.batch_id().0, manifest_bytes],
            )
            .map_err(|_| unavailable("ChangeBatch mutation preimages cannot be retained"))?;
        update_phase(
            &transaction,
            &record.batch_id().0,
            ChangeBatchExecutionPhase::IntentRetained,
            ChangeBatchExecutionPhase::PreimagesReady,
            &execution.event.occurred_at.0,
        )?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch mutation preimages cannot be committed"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Compare-and-sets one legal execution phase.
    ///
    /// # Errors
    ///
    /// Rejects stale expected phases and illegal transitions.
    pub fn transition(
        &mut self,
        batch_id: &ChangeBatchId,
        expected: ChangeBatchExecutionPhase,
        next: ChangeBatchExecutionPhase,
        now: &Instant,
    ) -> Result<(), ChangeBatchJournalError> {
        if !legal_phase_transition(expected, next) || now.0.is_empty() {
            return Err(invalid("ChangeBatch phase transition is invalid"));
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch phase transaction cannot begin"))?;
        update_phase(&transaction, &batch_id.0, expected, next, &now.0)?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch phase transaction cannot commit"))
    }

    /// Records completion of exactly the currently expected operation.
    ///
    /// # Errors
    ///
    /// Rejects another phase, skipped/repeated ordinals, or missing manifests.
    pub fn advance_operation(
        &mut self,
        batch_id: &ChangeBatchId,
        ordinal: u64,
        now: &Instant,
    ) -> Result<(), ChangeBatchJournalError> {
        let changed = self
            .connection
            .execute(
                "UPDATE change_batch_execution
                 SET next_operation = next_operation + 1, updated_at = ?3
                 WHERE batch_id = ?1 AND phase = ?2 AND next_operation = ?4
                   AND EXISTS (
                     SELECT 1 FROM change_batch_rollback_entry
                     WHERE batch_id = ?1 AND ordinal = ?4
                   )",
                params![
                    batch_id.0,
                    ChangeBatchExecutionPhase::Applying.as_str(),
                    now.0,
                    i64_from_u64(ordinal)?,
                ],
            )
            .map_err(|_| unavailable("ChangeBatch operation cursor cannot be persisted"))?;
        if changed != 1 {
            return Err(conflict("ChangeBatch operation cursor is stale"));
        }
        Ok(())
    }

    /// Classifies an interrupted current operation as before, after, or other
    /// and durably advances or enters rollback-required state as appropriate.
    ///
    /// # Errors
    ///
    /// Rejects a missing/currently unrelated operation or invalid observation.
    pub fn reconcile_interrupted_operation(
        &mut self,
        batch_id: &ChangeBatchId,
        ordinal: u64,
        observed: &FileStateFingerprint,
        now: &Instant,
    ) -> Result<ChangeBatchRecoveryState, ChangeBatchJournalError> {
        observed.validate()?;
        let entry = load_rollback_entry(&self.connection, &batch_id.0, ordinal)?
            .ok_or_else(|| conflict("ChangeBatch recovery entry does not exist"))?;
        let before = FileStateFingerprint {
            exists: entry.before_exists,
            digest: entry.before_digest.clone(),
            mode: entry.before_mode.clone(),
        };
        let after = FileStateFingerprint {
            exists: entry.after.exists,
            digest: entry.after.digest.clone(),
            mode: entry.after.mode.clone(),
        };
        let state = if observed == &before {
            ChangeBatchRecoveryState::Before
        } else if observed == &after {
            ChangeBatchRecoveryState::After
        } else {
            ChangeBatchRecoveryState::Other
        };
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch recovery transaction cannot begin"))?;
        let current = load_phase_and_operation(&transaction, &batch_id.0)?
            .ok_or_else(|| conflict("ChangeBatch recovery has no retained intent"))?;
        if current != (ChangeBatchExecutionPhase::Applying, ordinal) {
            return Err(conflict("ChangeBatch recovery cursor is stale"));
        }
        match state {
            ChangeBatchRecoveryState::Before => {}
            ChangeBatchRecoveryState::After => {
                transaction
                    .execute(
                        "UPDATE change_batch_execution
                         SET next_operation = next_operation + 1, updated_at = ?2
                         WHERE batch_id = ?1",
                        params![batch_id.0, now.0],
                    )
                    .map_err(|_| unavailable("ChangeBatch recovered cursor cannot be persisted"))?;
            }
            ChangeBatchRecoveryState::Other => update_phase(
                &transaction,
                &batch_id.0,
                ChangeBatchExecutionPhase::Applying,
                ChangeBatchExecutionPhase::RollbackRequired,
                &now.0,
            )?,
        }
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch recovery transaction cannot commit"))?;
        Ok(state)
    }

    /// Appends one canonical contiguous progress event to a durable outbox.
    ///
    /// # Errors
    ///
    /// Rejects changed identity, repeated changed bytes, gaps, or illegal state.
    pub fn append_progress(
        &mut self,
        event: &ChangeBatchProgressEvent,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        let bytes = canonical_bytes(event)?;
        validate_progress_contract(event, &bytes)?;
        let batch_id = &event.identity.batch_id.0;
        let authority = load_authority(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch progress has no retained intent"))?;
        if event.identity != authority {
            return Err(conflict("ChangeBatch progress authority changed"));
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch progress transaction cannot begin"))?;
        if let Some(existing) = transaction
            .query_row(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 AND sequence = ?2",
                params![batch_id, event.sequence],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch progress cannot be read"))?
        {
            return if existing == bytes {
                Ok(JournalRetention::Replay)
            } else {
                Err(conflict("ChangeBatch progress sequence changed on replay"))
            };
        }
        let events = load_progress_events(&transaction, batch_id)?;
        let mut ledger = ChangeBatchProgressLedger::new();
        for existing in &events {
            ledger
                .record(existing)
                .map_err(|_| corrupt("Stored ChangeBatch progress order is invalid"))?;
        }
        ledger
            .record(event)
            .map_err(|_| conflict("ChangeBatch progress order or state is invalid"))?;
        transaction
            .execute(
                "INSERT INTO change_batch_progress (batch_id, sequence, event_json, acknowledged)
                 VALUES (?1, ?2, ?3, 0)",
                params![batch_id, event.sequence, bytes],
            )
            .map_err(|_| unavailable("ChangeBatch progress cannot be persisted"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch progress transaction cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Atomically retains the final executor progress and its unique receipt.
    ///
    /// # Errors
    ///
    /// Rejects an invalid state/receipt pair, changed authority, a non-contiguous
    /// progress stream, or a same-batch replay with changed bytes.
    pub fn retain_final_progress_and_receipt(
        &mut self,
        event: &ChangeBatchProgressEvent,
        receipt: &ChangeBatchReceipt,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        let progress_bytes = canonical_bytes(event)?;
        validate_progress_contract(event, &progress_bytes)?;
        let receipt_bytes = canonical_bytes(receipt)?;
        let batch_id = &event.identity.batch_id.0;
        let authority = load_authority(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch final progress has no retained intent"))?;
        let base_revision = load_base_revision(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch final progress has no sealed base"))?;
        validate_receipt(event, receipt, &authority, &base_revision)?;

        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch final transaction cannot begin"))?;
        let existing_progress = transaction
            .query_row(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 AND sequence = ?2",
                params![batch_id, event.sequence],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch final progress cannot be read"))?;
        let existing_receipt = transaction
            .query_row(
                "SELECT receipt_json FROM change_batch_receipt_outbox WHERE batch_id = ?1",
                params![batch_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch final receipt cannot be read"))?;
        match (existing_progress, existing_receipt) {
            (Some(progress), Some(stored_receipt))
                if progress == progress_bytes && stored_receipt == receipt_bytes =>
            {
                return Ok(JournalRetention::Replay);
            }
            (Some(_), _) | (_, Some(_)) => {
                return Err(conflict(
                    "ChangeBatch final progress or receipt changed on replay",
                ));
            }
            (None, None) => {}
        }

        let events = load_progress_events(&transaction, batch_id)?;
        let mut ledger = ChangeBatchProgressLedger::new();
        for existing in &events {
            ledger
                .record(existing)
                .map_err(|_| corrupt("Stored ChangeBatch progress order is invalid"))?;
        }
        ledger
            .record(event)
            .map_err(|_| conflict("ChangeBatch final progress order or state is invalid"))?;
        let phase = receipt_phase(receipt);
        let changed = transaction
            .execute(
                "UPDATE change_batch_execution
                 SET receipt_json = ?2, phase = ?3, updated_at = ?4
                 WHERE batch_id = ?1 AND receipt_json IS NULL",
                params![batch_id, receipt_bytes, phase.as_str(), now.0],
            )
            .map_err(|_| unavailable("ChangeBatch final receipt cannot be retained"))?;
        if changed != 1 {
            return Err(conflict(
                "ChangeBatch execution already has another receipt",
            ));
        }
        transaction
            .execute(
                "INSERT INTO change_batch_progress (batch_id, sequence, event_json, acknowledged)
                 VALUES (?1, ?2, ?3, 0)",
                params![batch_id, event.sequence, progress_bytes],
            )
            .map_err(|_| unavailable("ChangeBatch final progress cannot be retained"))?;
        transaction
            .execute(
                "INSERT INTO change_batch_receipt_outbox (batch_id, receipt_json, acknowledged)
                 VALUES (?1, ?2, 0)",
                params![batch_id, receipt_bytes],
            )
            .map_err(|_| unavailable("ChangeBatch final receipt outbox cannot be retained"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch final transaction cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Atomically retains a terminal receipt and quarantines or closes its barrier.
    ///
    /// # Errors
    ///
    /// Rejects a stale workspace state, changed replay, invalid receipt, or an
    /// illegal terminal progress transition.
    #[allow(clippy::too_many_lines)]
    pub fn retain_terminal_workspace_receipt(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProgressEvent,
        receipt: &ChangeBatchReceipt,
        expected: ActiveBatchState,
        next: ActiveBatchState,
        now: &Instant,
    ) -> Result<JournalRetention, ChangeBatchJournalError> {
        if !valid_progress_barrier_transition(&event.state, expected, next) {
            return Err(invalid(
                "ChangeBatch terminal progress does not match the workspace transition",
            ));
        }
        let progress_bytes = canonical_bytes(event)?;
        validate_progress_contract(event, &progress_bytes)?;
        let receipt_bytes = canonical_bytes(receipt)?;
        let batch_id = &event.identity.batch_id.0;
        let authority = load_authority(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch terminal progress has no retained intent"))?;
        let base_revision = load_base_revision(&self.connection, batch_id)?
            .ok_or_else(|| conflict("ChangeBatch terminal progress has no sealed base"))?;
        validate_receipt(event, receipt, &authority, &base_revision)?;

        let transaction = self
            .connection
            .transaction()
            .map_err(|_| unavailable("ChangeBatch terminal workspace transaction cannot begin"))?;
        let barrier = load_workspace_barrier_row(&transaction, workspace_id)?
            .map(|row| workspace_barrier_from_row(workspace_id, row))
            .transpose()?
            .ok_or_else(|| conflict("ChangeBatch terminal workspace barrier is missing"))?;
        let existing_progress = transaction
            .query_row(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 AND sequence = ?2",
                params![batch_id, event.sequence],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch terminal workspace progress cannot be read"))?;
        let existing_receipt = transaction
            .query_row(
                "SELECT receipt_json FROM change_batch_receipt_outbox WHERE batch_id = ?1",
                params![batch_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch terminal workspace receipt cannot be read"))?;
        if barrier.state == next
            && barrier.active_batch_id.as_ref() == Some(&event.identity.batch_id)
            && existing_progress.as_deref() == Some(progress_bytes.as_slice())
            && existing_receipt.as_deref() == Some(receipt_bytes.as_slice())
        {
            transaction
                .commit()
                .map_err(|_| unavailable("ChangeBatch terminal workspace replay cannot commit"))?;
            return Ok(JournalRetention::Replay);
        }
        if barrier.state != expected
            || barrier.active_batch_id.as_ref() != Some(&event.identity.batch_id)
            || existing_progress.is_some()
            || existing_receipt.is_some()
        {
            return Err(conflict(
                "ChangeBatch terminal workspace compare-and-set failed",
            ));
        }
        validate_next_progress(&transaction, event)?;
        let phase = receipt_phase(receipt);
        let barrier_changed = transaction
            .execute(
                "UPDATE change_batch_workspace_barrier SET state = ?4, updated_at = ?5
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = ?3",
                params![
                    workspace_id,
                    batch_id,
                    expected.as_str(),
                    next.as_str(),
                    now.0
                ],
            )
            .map_err(|_| unavailable("ChangeBatch terminal workspace cannot move barrier"))?;
        let execution_changed = transaction
            .execute(
                "UPDATE change_batch_execution
                 SET receipt_json = ?2, phase = ?3, updated_at = ?4
                 WHERE batch_id = ?1 AND receipt_json IS NULL",
                params![batch_id, receipt_bytes, phase.as_str(), now.0],
            )
            .map_err(|_| {
                unavailable("ChangeBatch terminal workspace receipt cannot be retained")
            })?;
        if barrier_changed != 1 || execution_changed != 1 {
            return Err(conflict(
                "ChangeBatch terminal workspace compare-and-set failed",
            ));
        }
        transaction
            .execute(
                "INSERT INTO change_batch_progress (batch_id, sequence, event_json, acknowledged)
                 VALUES (?1, ?2, ?3, 0)",
                params![batch_id, event.sequence, progress_bytes],
            )
            .map_err(|_| {
                unavailable("ChangeBatch terminal workspace progress cannot be retained")
            })?;
        transaction
            .execute(
                "INSERT INTO change_batch_receipt_outbox (batch_id, receipt_json, acknowledged)
                 VALUES (?1, ?2, 0)",
                params![batch_id, receipt_bytes],
            )
            .map_err(|_| unavailable("ChangeBatch terminal workspace outbox cannot be retained"))?;
        transaction
            .commit()
            .map_err(|_| unavailable("ChangeBatch terminal workspace cannot commit"))?;
        Ok(JournalRetention::Inserted)
    }

    /// Returns the complete durable progress stream, including acknowledged rows.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored events or lifecycle order.
    pub fn progress_events(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Vec<ChangeBatchProgressEvent>, ChangeBatchJournalError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 ORDER BY sequence",
            )
            .map_err(|_| unavailable("ChangeBatch progress ledger cannot be opened"))?;
        let rows = statement
            .query_map(params![batch_id.0], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|_| unavailable("ChangeBatch progress ledger cannot be read"))?;
        let events = rows
            .map(|row| {
                let bytes =
                    row.map_err(|_| unavailable("ChangeBatch progress row cannot be read"))?;
                serde_json::from_slice(&bytes)
                    .map_err(|_| corrupt("ChangeBatch progress event is corrupt"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut ledger = ChangeBatchProgressLedger::new();
        for event in &events {
            ledger
                .record(event)
                .map_err(|_| corrupt("Stored ChangeBatch progress order is invalid"))?;
        }
        Ok(events)
    }

    /// Returns unacknowledged progress without consuming it.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored event bytes.
    pub fn pending_progress(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Vec<ChangeBatchProgressEvent>, ChangeBatchJournalError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT event_json FROM change_batch_progress
                 WHERE batch_id = ?1 AND acknowledged = 0 ORDER BY sequence",
            )
            .map_err(|_| unavailable("ChangeBatch progress outbox cannot be opened"))?;
        let rows = statement
            .query_map(params![batch_id.0], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|_| unavailable("ChangeBatch progress outbox cannot be read"))?;
        rows.map(|row| {
            let bytes = row.map_err(|_| unavailable("ChangeBatch progress row cannot be read"))?;
            serde_json::from_slice(&bytes)
                .map_err(|_| corrupt("ChangeBatch progress event is corrupt"))
        })
        .collect()
    }

    /// Acknowledges one exact durable progress sequence.
    ///
    /// # Errors
    ///
    /// Rejects a missing sequence.
    pub fn acknowledge_progress(
        &mut self,
        batch_id: &ChangeBatchId,
        sequence: i64,
    ) -> Result<(), ChangeBatchJournalError> {
        let changed = self
            .connection
            .execute(
                "UPDATE change_batch_progress SET acknowledged = 1
                 WHERE batch_id = ?1 AND sequence = ?2",
                params![batch_id.0, sequence],
            )
            .map_err(|_| unavailable("ChangeBatch progress acknowledgement cannot be persisted"))?;
        if changed != 1 {
            return Err(conflict("ChangeBatch progress acknowledgement is unknown"));
        }
        Ok(())
    }

    /// Returns the unacknowledged receipt without consuming it.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored receipt bytes.
    pub fn pending_receipt(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Option<ChangeBatchReceipt>, ChangeBatchJournalError> {
        self.connection
            .query_row(
                "SELECT receipt_json FROM change_batch_receipt_outbox
                 WHERE batch_id = ?1 AND acknowledged = 0",
                params![batch_id.0],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch receipt outbox cannot be read"))?
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|_| corrupt("ChangeBatch receipt is corrupt"))
            })
            .transpose()
    }

    /// Acknowledges the unique receipt while retaining its immutable bytes.
    ///
    /// # Errors
    ///
    /// Rejects a missing receipt.
    pub fn acknowledge_receipt(
        &mut self,
        batch_id: &ChangeBatchId,
    ) -> Result<(), ChangeBatchJournalError> {
        let changed = self
            .connection
            .execute(
                "UPDATE change_batch_receipt_outbox SET acknowledged = 1 WHERE batch_id = ?1",
                params![batch_id.0],
            )
            .map_err(|_| unavailable("ChangeBatch receipt acknowledgement cannot be persisted"))?;
        if changed != 1 {
            return Err(conflict("ChangeBatch receipt acknowledgement is unknown"));
        }
        Ok(())
    }

    fn persist_blob(
        &self,
        digest: &Sha256Digest,
        bytes: &[u8],
    ) -> Result<(), ChangeBatchJournalError> {
        let destination = blob_path(&self.blob_root, digest)?;
        if destination.exists() {
            let existing = fs::read(&destination)
                .map_err(|_| unavailable("ChangeBatch preimage blob cannot be read"))?;
            if existing == bytes && digest_bytes(&existing) == *digest {
                return Ok(());
            }
            return Err(corrupt(
                "ChangeBatch preimage blob changed under its digest",
            ));
        }
        let hex = digest
            .0
            .strip_prefix("sha256:")
            .ok_or_else(|| invalid("ChangeBatch preimage digest is invalid"))?;
        let temporary = self
            .blob_root
            .join(format!(".{hex}.{}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary)
            .map_err(|_| unavailable("ChangeBatch preimage temporary file cannot be created"))?;
        let write = file.write_all(bytes).and_then(|()| file.sync_all());
        if write.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(unavailable("ChangeBatch preimage blob cannot be persisted"));
        }
        fs::rename(&temporary, &destination)
            .map_err(|_| unavailable("ChangeBatch preimage blob cannot be installed"))?;
        sync_directory(&self.blob_root)?;
        Ok(())
    }
}

fn migrate_journal_schema(connection: &mut Connection) -> Result<(), ChangeBatchJournalError> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|_| corrupt("ChangeBatch journal schema version cannot be read"))?;
    match version {
        JOURNAL_SCHEMA_VERSION => validate_schema_v4(connection),
        3 => {
            let transaction = connection
                .transaction()
                .map_err(|_| unavailable("ChangeBatch journal migration cannot begin"))?;
            transaction
                .execute_batch(
                    "ALTER TABLE change_batch_phase_command
                       ADD COLUMN diagnostic_batch_json BLOB;
                     ALTER TABLE change_batch_phase_command
                       ADD COLUMN diagnostic_parse_failed INTEGER NOT NULL DEFAULT 0;
                     CREATE TABLE change_batch_diagnostic_baseline (
                       scope_digest TEXT NOT NULL,
                       workspace_revision TEXT NOT NULL,
                       baseline_json BLOB NOT NULL,
                       baseline_digest TEXT NOT NULL,
                       PRIMARY KEY (scope_digest, workspace_revision)
                     );
                     CREATE TABLE change_batch_diagnostic_evaluation (
                       batch_id TEXT PRIMARY KEY NOT NULL,
                       evaluation_json BLOB NOT NULL,
                       evaluation_digest TEXT NOT NULL,
                       created_at TEXT NOT NULL,
                       FOREIGN KEY (batch_id) REFERENCES change_batch_execution(batch_id)
                     );",
                )
                .map_err(|_| unavailable("ChangeBatch journal v3 migration cannot apply"))?;
            transaction
                .pragma_update(None, "user_version", JOURNAL_SCHEMA_VERSION)
                .map_err(|_| unavailable("ChangeBatch journal version cannot be persisted"))?;
            transaction
                .commit()
                .map_err(|_| unavailable("ChangeBatch journal migration cannot commit"))?;
            validate_schema_v4(connection)
        }
        0..=2 => {
            let transaction = connection
                .transaction()
                .map_err(|_| unavailable("ChangeBatch journal migration cannot begin"))?;
            ChangeBatchJournal::initialize_schema_v4(&transaction)?;
            transaction
                .pragma_update(None, "user_version", JOURNAL_SCHEMA_VERSION)
                .map_err(|_| unavailable("ChangeBatch journal version cannot be persisted"))?;
            transaction
                .commit()
                .map_err(|_| unavailable("ChangeBatch journal migration cannot commit"))?;
            validate_schema_v4(connection)
        }
        _ => Err(corrupt("ChangeBatch journal schema version is unsupported")),
    }
}

fn retain_intent_on(
    connection: &Connection,
    event: &ChangeBatchProposalEvent,
    base_revision: &WorkspaceRevision,
    plan_digest: &Sha256Digest,
    now: &Instant,
) -> Result<JournalRetention, ChangeBatchJournalError> {
    validate_event(event)?;
    let prepared = prepare_change_batch(event, ChangeBatchPolicy::default())
        .map_err(|_| invalid("ChangeBatch proposal cannot be planned"))?;
    if prepared.plan_digest() != plan_digest
        || !valid_workspace_revision(base_revision)
        || now.0.is_empty()
    {
        return Err(invalid("ChangeBatch plan identity is invalid"));
    }
    let event_json = canonical_bytes(event)?;
    let proposal_json = canonical_bytes(&event.proposal)?;
    let authority_json = canonical_bytes(&event.identity)?;
    let intent_digest = digest_bytes(&event_json);
    let proposal_digest = digest_bytes(&proposal_json);
    let authority_digest = digest_bytes(&authority_json);
    let batch_id = &event.identity.batch_id.0;
    if let Some(existing) = load_execution_row(connection, batch_id)? {
        validate_execution_row(&existing)?;
        if existing.proposal_event_json == event_json
            && existing.intent_digest == intent_digest
            && existing.proposal_digest == proposal_digest
            && existing.authority_json == authority_json
            && existing.authority_digest == authority_digest
            && existing.base_revision == base_revision.0
            && existing.plan_digest == *plan_digest
        {
            return Ok(JournalRetention::Replay);
        }
        return Err(conflict(
            "ChangeBatch id was reused with changed intent bytes",
        ));
    }
    connection
        .execute(
            "INSERT INTO change_batch_execution (
               batch_id, proposal_event_json, intent_digest, proposal_digest,
               authority_json, authority_digest, base_revision, plan_digest,
               phase, next_operation, receipt_json, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, NULL, ?10, ?10)",
            params![
                batch_id,
                event_json,
                intent_digest.0,
                proposal_digest.0,
                authority_json,
                authority_digest.0,
                base_revision.0,
                plan_digest.0,
                ChangeBatchExecutionPhase::IntentRetained.as_str(),
                now.0,
            ],
        )
        .map_err(|_| unavailable("ChangeBatch intent cannot be persisted"))?;
    Ok(JournalRetention::Inserted)
}

type WorkspaceBarrierRow = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

fn load_workspace_barrier_row(
    connection: &Connection,
    workspace_id: &str,
) -> Result<Option<WorkspaceBarrierRow>, ChangeBatchJournalError> {
    connection
        .query_row(
            "SELECT accepted_revision, active_batch_id, state,
                    checkpoint_revision, checkpoint_delta_digest
             FROM change_batch_workspace_barrier WHERE workspace_id = ?1",
            params![workspace_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| unavailable("ChangeBatch workspace barrier cannot be read"))
}

fn workspace_barrier_from_row(
    workspace_id: &str,
    row: WorkspaceBarrierRow,
) -> Result<WorkspaceBatchBarrier, ChangeBatchJournalError> {
    let (accepted_revision, active_batch_id, state, checkpoint_revision, checkpoint_delta_digest) =
        row;
    let barrier = WorkspaceBatchBarrier {
        workspace_id: workspace_id.to_owned(),
        accepted_revision: WorkspaceRevision(accepted_revision),
        active_batch_id: active_batch_id.map(ChangeBatchId),
        state: ActiveBatchState::parse(&state)?,
        checkpoint_revision: checkpoint_revision.map(WorkspaceRevision),
        checkpoint_delta_digest: checkpoint_delta_digest.map(Sha256Digest),
    };
    validate_workspace_barrier(&barrier)?;
    Ok(barrier)
}

fn validate_schema_v4(connection: &Connection) -> Result<(), ChangeBatchJournalError> {
    for table in [
        "change_batch_execution",
        "change_batch_progress",
        "change_batch_rollback_entry",
        "change_batch_receipt_outbox",
        "change_batch_mutation_preimage_manifest",
        "change_batch_workspace_barrier",
        "change_batch_workspace_restore_intent",
        "change_batch_phase_run",
        "change_batch_phase_command",
        "change_batch_diagnostic_baseline",
        "change_batch_diagnostic_evaluation",
    ] {
        let exists = connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |_| Ok(()),
            )
            .optional()
            .map_err(|_| corrupt("ChangeBatch journal schema cannot be inspected"))?
            .is_some();
        if !exists {
            return Err(corrupt("ChangeBatch journal v4 table is missing"));
        }
    }
    let mut statement = connection
        .prepare("PRAGMA table_info(change_batch_workspace_restore_intent)")
        .map_err(|_| corrupt("ChangeBatch restore-intent schema cannot be inspected"))?;
    let columns = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|_| corrupt("ChangeBatch restore-intent schema cannot be read"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| corrupt("ChangeBatch restore-intent column cannot be read"))?;
    if columns
        != [
            ("workspace_id".to_owned(), 1, 1),
            ("expected_current".to_owned(), 1, 2),
            ("target_revision".to_owned(), 1, 3),
        ]
    {
        return Err(corrupt("ChangeBatch restore-intent schema is not v3"));
    }
    if table_columns(connection, "change_batch_phase_run")?
        != [
            ("batch_id".to_owned(), 1, 1),
            ("selection_json".to_owned(), 1, 0),
            ("selection_digest".to_owned(), 1, 0),
            ("next_command".to_owned(), 1, 0),
            ("normalizer_receipt_json".to_owned(), 0, 0),
            ("validation_receipt_json".to_owned(), 0, 0),
            ("created_at".to_owned(), 1, 0),
            ("updated_at".to_owned(), 1, 0),
        ]
        || table_columns(connection, "change_batch_phase_command")?
            != [
                ("batch_id".to_owned(), 1, 1),
                ("ordinal".to_owned(), 1, 2),
                ("command_id".to_owned(), 1, 0),
                ("receipt_json".to_owned(), 1, 0),
                ("diagnostic_batch_json".to_owned(), 0, 0),
                ("diagnostic_parse_failed".to_owned(), 1, 0),
            ]
    {
        return Err(corrupt("ChangeBatch phase schema is not v4"));
    }
    Ok(())
}

fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<Vec<(String, i64, i64)>, ChangeBatchJournalError> {
    let sql = format!("PRAGMA table_info({table})");
    let mut statement = connection
        .prepare(&sql)
        .map_err(|_| corrupt("ChangeBatch phase schema cannot be inspected"))?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|_| corrupt("ChangeBatch phase schema cannot be read"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| corrupt("ChangeBatch phase schema column cannot be read"))
}

fn validate_workspace_barrier(
    barrier: &WorkspaceBatchBarrier,
) -> Result<(), ChangeBatchJournalError> {
    if barrier.workspace_id.is_empty()
        || !valid_workspace_revision(&barrier.accepted_revision)
        || barrier.state.has_active_batch() != barrier.active_batch_id.is_some()
        || barrier.checkpoint_revision.is_some() != barrier.checkpoint_delta_digest.is_some()
        || barrier
            .checkpoint_revision
            .as_ref()
            .is_some_and(|revision| !valid_workspace_revision(revision))
        || barrier
            .checkpoint_delta_digest
            .as_ref()
            .is_some_and(|digest| !valid_digest(digest))
        || matches!(
            barrier.state,
            ActiveBatchState::Checkpointed
                | ActiveBatchState::ValidationPending
                | ActiveBatchState::ObservationPending
        ) && barrier.checkpoint_revision.is_none()
    {
        return Err(corrupt("ChangeBatch workspace barrier is inconsistent"));
    }
    Ok(())
}

const fn legal_workspace_transition(from: ActiveBatchState, to: ActiveBatchState) -> bool {
    matches!(
        (from, to),
        (
            ActiveBatchState::Applying,
            ActiveBatchState::CheckpointPending
                | ActiveBatchState::RollbackPending
                | ActiveBatchState::Quarantined
        ) | (
            ActiveBatchState::CheckpointPending | ActiveBatchState::RollbackPending,
            ActiveBatchState::Quarantined
        ) | (
            ActiveBatchState::Checkpointed,
            ActiveBatchState::ValidationPending | ActiveBatchState::RollbackPending
        ) | (
            ActiveBatchState::ValidationPending,
            ActiveBatchState::ObservationPending
                | ActiveBatchState::RollbackPending
                | ActiveBatchState::RepairRequired
        ) | (
            ActiveBatchState::ObservationPending,
            ActiveBatchState::RollbackPending | ActiveBatchState::RepairRequired
        ) | (
            ActiveBatchState::RollbackPending,
            ActiveBatchState::RolledBack
        ) | (
            ActiveBatchState::RolledBack,
            ActiveBatchState::RepairRequired
        )
    )
}

const fn valid_progress_barrier_transition(
    progress: &ChangeBatchProgressState,
    from: ActiveBatchState,
    to: ActiveBatchState,
) -> bool {
    matches!(
        (progress, from, to),
        (
            ChangeBatchProgressState::RollbackStarted,
            ActiveBatchState::Applying
                | ActiveBatchState::Checkpointed
                | ActiveBatchState::ValidationPending
                | ActiveBatchState::ObservationPending,
            ActiveBatchState::RollbackPending
        ) | (
            ChangeBatchProgressState::RolledBack,
            ActiveBatchState::RollbackPending,
            ActiveBatchState::RolledBack
        ) | (
            ChangeBatchProgressState::RepairRequired,
            ActiveBatchState::RolledBack | ActiveBatchState::ValidationPending,
            ActiveBatchState::RepairRequired
        ) | (
            ChangeBatchProgressState::InfrastructureFailed,
            ActiveBatchState::Applying
                | ActiveBatchState::CheckpointPending
                | ActiveBatchState::RollbackPending,
            ActiveBatchState::Quarantined
        ) | (
            ChangeBatchProgressState::ValidationStarted,
            ActiveBatchState::Checkpointed,
            ActiveBatchState::ValidationPending
        ) | (
            ChangeBatchProgressState::ValidationCompleted,
            ActiveBatchState::ValidationPending,
            ActiveBatchState::ValidationPending
        ) | (
            ChangeBatchProgressState::ObservationRequested,
            ActiveBatchState::ValidationPending,
            ActiveBatchState::ObservationPending
        ) | (
            ChangeBatchProgressState::ObservationCompleted,
            ActiveBatchState::ObservationPending,
            ActiveBatchState::ObservationPending
        )
    )
}

fn validate_next_progress(
    connection: &Transaction<'_>,
    event: &ChangeBatchProgressEvent,
) -> Result<(), ChangeBatchJournalError> {
    let events = load_progress_events(connection, &event.identity.batch_id.0)?;
    let mut ledger = ChangeBatchProgressLedger::new();
    for existing in &events {
        ledger
            .record(existing)
            .map_err(|_| corrupt("Stored ChangeBatch progress order is invalid"))?;
    }
    ledger
        .record(event)
        .map_err(|_| conflict("ChangeBatch progress order or state is invalid"))
}

fn receipt_delta_matches(
    connection: &Transaction<'_>,
    batch_id: &ChangeBatchId,
    expected: &Sha256Digest,
) -> Result<bool, ChangeBatchJournalError> {
    let Some(bytes) = connection
        .query_row(
            "SELECT receipt_json FROM change_batch_receipt_outbox WHERE batch_id = ?1",
            params![batch_id.0],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| unavailable("ChangeBatch accepted receipt cannot be read"))?
    else {
        return Ok(false);
    };
    let receipt: ChangeBatchReceipt = serde_json::from_slice(&bytes)
        .map_err(|_| corrupt("ChangeBatch accepted receipt is corrupt"))?;
    Ok(receipt.delta_digest.as_ref() == Some(expected))
}

impl ExecutionJournalPort for ChangeBatchJournal {
    fn persist_preimages_and_sync(
        &mut self,
        record: &PreparedPreimageJournalRecord,
    ) -> Result<(), ExecutionJournalError> {
        self.retain_mutation_preimages(record)
            .map(|_| ())
            .map_err(|error| match error.code() {
                ChangeBatchJournalErrorCode::Invalid | ChangeBatchJournalErrorCode::Conflict => {
                    ExecutionJournalError::Conflict
                }
                ChangeBatchJournalErrorCode::Corrupt => ExecutionJournalError::Corrupt,
                ChangeBatchJournalErrorCode::Capacity => ExecutionJournalError::Capacity,
                ChangeBatchJournalErrorCode::Unavailable => ExecutionJournalError::Unavailable,
            })
    }
}

impl WorkspaceTreeRestoreJournalPort for ChangeBatchJournal {
    fn persist_restore_intent_and_sync(
        &mut self,
        intent: &WorkspaceTreeRestoreIntent,
    ) -> Result<(), WorkspaceTreeError> {
        let existing = self
            .connection
            .query_row(
                "SELECT 1 FROM change_batch_workspace_restore_intent
                 WHERE workspace_id = ?1 AND expected_current = ?2 AND target_revision = ?3",
                params![
                    intent.workspace_id(),
                    intent.expected_current().0,
                    intent.target().0
                ],
                |_| Ok(()),
            )
            .optional()
            .map_err(|_| WorkspaceTreeError::journal())?;
        if existing.is_some() {
            return Ok(());
        }
        self.connection
            .execute(
                "INSERT INTO change_batch_workspace_restore_intent
                 (workspace_id, expected_current, target_revision) VALUES (?1, ?2, ?3)",
                params![
                    intent.workspace_id(),
                    intent.expected_current().0,
                    intent.target().0
                ],
            )
            .map_err(|_| WorkspaceTreeError::journal())?;
        Ok(())
    }
}

struct ExecutionRow {
    proposal_event_json: Vec<u8>,
    intent_digest: Sha256Digest,
    proposal_digest: Sha256Digest,
    authority_json: Vec<u8>,
    authority_digest: Sha256Digest,
    base_revision: String,
    plan_digest: Sha256Digest,
    phase: ChangeBatchExecutionPhase,
    next_operation: u64,
    receipt_json: Option<Vec<u8>>,
}

fn valid_phase_selection(selection: &ValidationProfileSelection) -> bool {
    if selection.executable {
        selection.configuration_digest.is_some()
            && !selection.command_ids.is_empty()
            && selection.command_ids.len() <= 64
    } else {
        selection.configuration_digest.is_none() && selection.command_ids.is_empty()
    }
}

fn diagnostic_scope_digest(
    selection: &ValidationProfileSelection,
) -> Result<Sha256Digest, ChangeBatchJournalError> {
    let configuration_digest = selection
        .configuration_digest
        .as_ref()
        .ok_or_else(|| conflict("ChangeBatch diagnostic selection is advisory"))?;
    canonical_bytes(&DiagnosticScope {
        configuration_digest,
        profile: &selection.profile,
        command_ids: &selection.command_ids,
    })
    .map(|bytes| digest_bytes(&bytes))
}

fn decode_diagnostic_baseline(
    bytes: &[u8],
    digest: &str,
    revision: &WorkspaceRevision,
) -> Result<DiagnosticBaseline, ChangeBatchJournalError> {
    if digest_bytes(bytes).0 != digest {
        return Err(corrupt("ChangeBatch diagnostic baseline digest changed"));
    }
    let baseline: DiagnosticBaseline = serde_json::from_slice(bytes)
        .map_err(|_| corrupt("ChangeBatch diagnostic baseline is corrupt"))?;
    if canonical_bytes(&baseline)? != bytes
        || baseline.workspace_revision != *revision
        || validate_diagnostic_baseline(&baseline).is_err()
    {
        return Err(corrupt("ChangeBatch diagnostic baseline is invalid"));
    }
    Ok(baseline)
}

fn validate_diagnostic_evaluation(
    evaluation: &ValidationDiagnosticEvaluation,
) -> Result<(), ChangeBatchJournalError> {
    if evaluation.disposition.is_empty()
        || evaluation
            .reason_code
            .as_ref()
            .is_some_and(String::is_empty)
        || evaluation.parser_failed == evaluation.result.is_some()
        || evaluation
            .baseline
            .as_ref()
            .is_some_and(|baseline| baseline.workspace_revision != evaluation.base_revision)
        || evaluation
            .result
            .as_ref()
            .is_some_and(|result| result.workspace_revision != evaluation.result_revision)
        || evaluation
            .baseline
            .as_ref()
            .is_some_and(|baseline| validate_diagnostic_baseline(baseline).is_err())
        || evaluation
            .result
            .as_ref()
            .is_some_and(|result| validate_diagnostic_baseline(result).is_err())
    {
        return Err(invalid("ChangeBatch diagnostic evaluation is invalid"));
    }
    if let (Some(baseline), Some(result), Some(comparison)) = (
        evaluation.baseline.as_ref(),
        evaluation.result.as_ref(),
        evaluation.comparison.as_ref(),
    ) {
        return validate_diagnostic_baseline_comparison(comparison, baseline, result)
            .map_err(|_| invalid("ChangeBatch diagnostic comparison is invalid"));
    }
    if evaluation.comparison.is_none()
        && (evaluation.result.is_none() || evaluation.baseline.is_none())
    {
        Ok(())
    } else {
        Err(invalid("ChangeBatch diagnostic comparison is invalid"))
    }
}

fn validate_retained_diagnostic_decision(
    evaluation: &ValidationDiagnosticEvaluation,
    status: &winwincode_execution_port::generated::ValidationReceiptStatus,
) -> Result<(), ChangeBatchJournalError> {
    let expected = decide_validation_diagnostics(
        status,
        evaluation.comparison.as_ref(),
        evaluation.parser_failed,
    );
    let matches = match expected {
        ValidationDiagnosticDisposition::Pass => {
            evaluation.disposition == "pass" && evaluation.reason_code.is_none()
        }
        ValidationDiagnosticDisposition::BaselineUnavailable => {
            evaluation.disposition == "baseline_unavailable" && evaluation.reason_code.is_none()
        }
        ValidationDiagnosticDisposition::RepairRequired { reason_code } => {
            evaluation.disposition == "repair_required"
                && evaluation.reason_code.as_deref() == Some(reason_code)
        }
    };
    if matches {
        Ok(())
    } else {
        Err(invalid("ChangeBatch diagnostic disposition is invalid"))
    }
}

fn load_phase_record(
    connection: &Connection,
    batch_id: &ChangeBatchId,
) -> Result<Option<ChangeBatchPhaseRecord>, ChangeBatchJournalError> {
    let Some((selection_bytes, selection_digest, next_command, normalizer, validation)) =
        connection
            .query_row(
                "SELECT selection_json, selection_digest, next_command,
                    normalizer_receipt_json, validation_receipt_json
             FROM change_batch_phase_run WHERE batch_id = ?1",
                params![batch_id.0],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable("ChangeBatch phase record cannot be read"))?
    else {
        return Ok(None);
    };
    if digest_bytes(&selection_bytes).0 != selection_digest {
        return Err(corrupt("ChangeBatch validation selection digest changed"));
    }
    let selection: ValidationProfileSelection = serde_json::from_slice(&selection_bytes)
        .map_err(|_| corrupt("ChangeBatch validation selection is corrupt"))?;
    if canonical_bytes(&selection)? != selection_bytes || !valid_phase_selection(&selection) {
        return Err(corrupt("ChangeBatch validation selection is invalid"));
    }
    let next_command = usize::try_from(next_command)
        .map_err(|_| corrupt("ChangeBatch validation cursor is invalid"))?;
    if next_command > selection.command_ids.len() {
        return Err(corrupt("ChangeBatch validation cursor exceeds selection"));
    }
    let (command_receipts, diagnostic_batches, diagnostic_parse_failures) =
        load_phase_commands(connection, batch_id, &selection, next_command)?;
    let normalizer_receipt = decode_optional_phase_receipt::<NormalizerReceipt>(normalizer)?;
    if let Some(receipt) = normalizer_receipt.as_ref() {
        validate_normalizer_receipt_binding(
            receipt,
            &receipt.base_revision,
            receipt.result_revision.as_ref(),
        )
        .map_err(|_| corrupt("ChangeBatch normalizer receipt is invalid"))?;
    }
    let validation_receipt = decode_optional_phase_receipt::<ValidationReceipt>(validation)?;
    if let Some(receipt) = validation_receipt.as_ref() {
        validate_validation_receipt_binding(
            receipt,
            &receipt.base_revision,
            receipt.result_revision.as_ref(),
        )
        .map_err(|_| corrupt("ChangeBatch validation receipt is invalid"))?;
    }
    Ok(Some(ChangeBatchPhaseRecord {
        selection,
        command_receipts,
        diagnostic_batches,
        diagnostic_parse_failures,
        normalizer_receipt,
        validation_receipt,
    }))
}

type LoadedPhaseCommands = (
    Vec<PhaseProcessReceipt>,
    Vec<Option<DiagnosticParseBatch>>,
    Vec<bool>,
);

fn load_phase_commands(
    connection: &Connection,
    batch_id: &ChangeBatchId,
    selection: &ValidationProfileSelection,
    next_command: usize,
) -> Result<LoadedPhaseCommands, ChangeBatchJournalError> {
    let mut statement = connection
        .prepare(
            "SELECT ordinal, command_id, receipt_json, diagnostic_batch_json,
                    diagnostic_parse_failed
             FROM change_batch_phase_command
             WHERE batch_id = ?1 ORDER BY ordinal",
        )
        .map_err(|_| unavailable("ChangeBatch validation commands cannot be opened"))?;
    let rows = statement
        .query_map(params![batch_id.0], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|_| unavailable("ChangeBatch validation commands cannot be read"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| unavailable("ChangeBatch validation command cannot be read"))?;
    if rows.len() != next_command {
        return Err(corrupt("ChangeBatch validation command cursor has a gap"));
    }
    let loaded = rows
        .into_iter()
        .enumerate()
        .map(
            |(expected, (ordinal, command_id, bytes, diagnostic_bytes, parse_failed))| {
                if usize::try_from(ordinal).ok() != Some(expected)
                    || selection.command_ids.get(expected) != Some(&command_id)
                {
                    return Err(corrupt("ChangeBatch validation command order changed"));
                }
                let receipt: PhaseProcessReceipt = serde_json::from_slice(&bytes)
                    .map_err(|_| corrupt("ChangeBatch validation command receipt is corrupt"))?;
                if receipt.name != command_id || canonical_bytes(&receipt)? != bytes {
                    return Err(corrupt("ChangeBatch validation command receipt changed"));
                }
                let diagnostic = diagnostic_bytes
                    .map(|bytes| {
                        let persisted: PersistedDiagnosticParseBatch =
                            serde_json::from_slice(&bytes).map_err(|_| {
                                corrupt("ChangeBatch diagnostic snapshot is corrupt")
                            })?;
                        if canonical_bytes(&persisted)? != bytes {
                            return Err(corrupt("ChangeBatch diagnostic snapshot changed"));
                        }
                        Ok(DiagnosticParseBatch::from(persisted))
                    })
                    .transpose()?;
                if !matches!(parse_failed, 0 | 1) || (diagnostic.is_some() && parse_failed == 1) {
                    return Err(corrupt("ChangeBatch diagnostic parse state is invalid"));
                }
                Ok((receipt, diagnostic, parse_failed == 1))
            },
        )
        .collect::<Result<Vec<_>, _>>()?;
    let mut receipts = Vec::with_capacity(loaded.len());
    let mut diagnostic_batches = Vec::with_capacity(loaded.len());
    let mut diagnostic_parse_failures = Vec::with_capacity(loaded.len());
    for (receipt, diagnostic, parse_failed) in loaded {
        receipts.push(receipt);
        diagnostic_batches.push(diagnostic);
        diagnostic_parse_failures.push(parse_failed);
    }
    Ok((receipts, diagnostic_batches, diagnostic_parse_failures))
}

fn decode_optional_phase_receipt<T>(
    bytes: Option<Vec<u8>>,
) -> Result<Option<T>, ChangeBatchJournalError>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    bytes
        .map(|bytes| {
            let value = serde_json::from_slice::<T>(&bytes)
                .map_err(|_| corrupt("ChangeBatch phase receipt is corrupt"))?;
            if canonical_bytes(&value)? != bytes {
                return Err(corrupt("ChangeBatch phase receipt bytes changed"));
            }
            Ok(value)
        })
        .transpose()
}

fn retain_phase_receipt<T: Serialize>(
    connection: &Connection,
    batch_id: &ChangeBatchId,
    column: &str,
    receipt: &T,
    now: &Instant,
) -> Result<JournalRetention, ChangeBatchJournalError> {
    if now.0.is_empty() {
        return Err(invalid("ChangeBatch phase receipt time is invalid"));
    }
    let bytes = canonical_bytes(receipt)?;
    let (read_sql, write_sql) = match column {
        "normalizer_receipt_json" => (
            "SELECT normalizer_receipt_json FROM change_batch_phase_run WHERE batch_id = ?1",
            "UPDATE change_batch_phase_run SET normalizer_receipt_json = ?2, updated_at = ?3
             WHERE batch_id = ?1 AND normalizer_receipt_json IS NULL",
        ),
        "validation_receipt_json" => (
            "SELECT validation_receipt_json FROM change_batch_phase_run WHERE batch_id = ?1",
            "UPDATE change_batch_phase_run SET validation_receipt_json = ?2, updated_at = ?3
             WHERE batch_id = ?1 AND validation_receipt_json IS NULL",
        ),
        _ => return Err(invalid("ChangeBatch phase receipt column is invalid")),
    };
    let existing = connection
        .query_row(read_sql, params![batch_id.0], |row| {
            row.get::<_, Option<Vec<u8>>>(0)
        })
        .optional()
        .map_err(|_| unavailable("ChangeBatch phase receipt cannot be read"))?
        .ok_or_else(|| conflict("ChangeBatch phase selection is missing"))?;
    if let Some(existing) = existing {
        return if existing == bytes {
            Ok(JournalRetention::Replay)
        } else {
            Err(conflict("ChangeBatch phase receipt changed on replay"))
        };
    }
    let changed = connection
        .execute(write_sql, params![batch_id.0, bytes, now.0])
        .map_err(|_| unavailable("ChangeBatch phase receipt cannot be persisted"))?;
    if changed != 1 {
        return Err(conflict("ChangeBatch phase receipt compare-and-set failed"));
    }
    Ok(JournalRetention::Inserted)
}

fn load_execution_row(
    connection: &Connection,
    batch_id: &str,
) -> Result<Option<ExecutionRow>, ChangeBatchJournalError> {
    connection
        .query_row(
            "SELECT proposal_event_json, intent_digest, proposal_digest, authority_json,
                    authority_digest, base_revision, plan_digest, phase,
                    next_operation, receipt_json
             FROM change_batch_execution WHERE batch_id = ?1",
            params![batch_id],
            |row| {
                let next_operation = row.get::<_, i64>(8)?;
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    next_operation,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                ))
            },
        )
        .optional()
        .map_err(|_| unavailable("ChangeBatch execution record cannot be read"))?
        .map(|row| {
            let next_operation = u64::try_from(row.8)
                .map_err(|_| corrupt("ChangeBatch operation cursor is invalid"))?;
            Ok(ExecutionRow {
                proposal_event_json: row.0,
                intent_digest: Sha256Digest(row.1),
                proposal_digest: Sha256Digest(row.2),
                authority_json: row.3,
                authority_digest: Sha256Digest(row.4),
                base_revision: row.5,
                plan_digest: Sha256Digest(row.6),
                phase: ChangeBatchExecutionPhase::parse(&row.7)?,
                next_operation,
                receipt_json: row.9,
            })
        })
        .transpose()
}

fn validate_execution_row(row: &ExecutionRow) -> Result<(), ChangeBatchJournalError> {
    let event: ChangeBatchProposalEvent = serde_json::from_slice(&row.proposal_event_json)
        .map_err(|_| corrupt("ChangeBatch proposal event is corrupt"))?;
    validate_event(&event).map_err(|_| corrupt("ChangeBatch proposal event is invalid"))?;
    let authority: ChangeBatchIdentity = serde_json::from_slice(&row.authority_json)
        .map_err(|_| corrupt("ChangeBatch authority is corrupt"))?;
    if event.identity != authority
        || digest_bytes(&row.proposal_event_json) != row.intent_digest
        || digest_bytes(&canonical_bytes(&event.proposal)?) != row.proposal_digest
        || digest_bytes(&row.authority_json) != row.authority_digest
        || !valid_workspace_revision(&WorkspaceRevision(row.base_revision.clone()))
        || !valid_digest(&row.plan_digest)
    {
        return Err(corrupt("ChangeBatch durable identity facts changed"));
    }
    if let Some(receipt) = row.receipt_json.as_ref() {
        let receipt: ChangeBatchReceipt = serde_json::from_slice(receipt)
            .map_err(|_| corrupt("ChangeBatch receipt is corrupt"))?;
        validate_receipt_without_progress(
            &receipt,
            &authority,
            &WorkspaceRevision(row.base_revision.clone()),
        )
        .map_err(|_| corrupt("ChangeBatch receipt semantics changed"))?;
    }
    Ok(())
}

fn validate_receipt(
    event: &ChangeBatchProgressEvent,
    receipt: &ChangeBatchReceipt,
    authority: &ChangeBatchIdentity,
    base_revision: &WorkspaceRevision,
) -> Result<(), ChangeBatchJournalError> {
    validate_receipt_without_progress(receipt, authority, base_revision)?;
    let state_matches = matches!(
        (&event.state, &receipt.status),
        (
            ChangeBatchProgressState::Applied,
            ChangeBatchReceiptStatus::Applied
        ) | (
            ChangeBatchProgressState::RepairRequired,
            ChangeBatchReceiptStatus::Rejected
        ) | (
            ChangeBatchProgressState::InfrastructureFailed,
            ChangeBatchReceiptStatus::PartiallyApplied | ChangeBatchReceiptStatus::StateUncertain
        )
    );
    if event.identity != *authority || receipt.identity != *authority || !state_matches {
        return Err(invalid(
            "ChangeBatch final progress and receipt do not match",
        ));
    }
    Ok(())
}

fn validate_receipt_without_progress(
    receipt: &ChangeBatchReceipt,
    authority: &ChangeBatchIdentity,
    base_revision: &WorkspaceRevision,
) -> Result<(), ChangeBatchJournalError> {
    let bytes = canonical_bytes(receipt)?;
    let decoded: ChangeBatchReceipt = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("ChangeBatch receipt is non-canonical"))?;
    let canonical_files = canonical_applied_file_summaries(&receipt.files)
        .map_err(|_| invalid("ChangeBatch receipt file summaries are invalid"))?;
    if decoded != *receipt
        || receipt.identity != *authority
        || receipt.base_revision != *base_revision
        || canonical_files != receipt.files
    {
        return Err(invalid("ChangeBatch receipt identity is invalid"));
    }
    match receipt.status {
        ChangeBatchReceiptStatus::Applied
        | ChangeBatchReceiptStatus::Rejected
        | ChangeBatchReceiptStatus::PartiallyApplied => {
            let digest = derive_delta_digest(&receipt.files)
                .map_err(|_| invalid("ChangeBatch receipt delta is invalid"))?;
            if !receipt.delta_exact
                || receipt.delta_digest.as_ref() != Some(&digest)
                || receipt.result_revision.is_none()
            {
                return Err(invalid("ChangeBatch exact receipt is incomplete"));
            }
        }
        ChangeBatchReceiptStatus::StateUncertain => {
            if receipt.delta_exact
                || receipt.delta_digest.is_some()
                || receipt.result_revision.is_some()
            {
                return Err(invalid("ChangeBatch uncertain receipt claims exact state"));
            }
        }
    }
    Ok(())
}

const fn receipt_phase(receipt: &ChangeBatchReceipt) -> ChangeBatchExecutionPhase {
    match receipt.status {
        ChangeBatchReceiptStatus::Applied => ChangeBatchExecutionPhase::Applied,
        ChangeBatchReceiptStatus::Rejected => ChangeBatchExecutionPhase::RolledBack,
        ChangeBatchReceiptStatus::PartiallyApplied | ChangeBatchReceiptStatus::StateUncertain => {
            ChangeBatchExecutionPhase::StateUncertain
        }
    }
}

fn execution_record(
    row: &ExecutionRow,
) -> Result<ChangeBatchJournalRecord, ChangeBatchJournalError> {
    validate_execution_row(row)?;
    let event = serde_json::from_slice(&row.proposal_event_json)
        .map_err(|_| corrupt("ChangeBatch proposal event is corrupt"))?;
    let receipt = row
        .receipt_json
        .as_ref()
        .map(|bytes| {
            serde_json::from_slice(bytes).map_err(|_| corrupt("ChangeBatch receipt is corrupt"))
        })
        .transpose()?;
    Ok(ChangeBatchJournalRecord {
        event,
        intent_digest: row.intent_digest.clone(),
        proposal_digest: row.proposal_digest.clone(),
        authority_digest: row.authority_digest.clone(),
        base_revision: WorkspaceRevision(row.base_revision.clone()),
        plan_digest: row.plan_digest.clone(),
        phase: row.phase,
        next_operation: row.next_operation,
        receipt,
    })
}

fn prepare_preimages(
    preimages: &[RollbackPreimage],
) -> Result<PreparedRollbackEntries, ChangeBatchJournalError> {
    if preimages.is_empty() {
        return Err(invalid("ChangeBatch rollback manifest is empty"));
    }
    preimages
        .iter()
        .enumerate()
        .map(|(expected, preimage)| {
            if usize::try_from(preimage.ordinal).ok() != Some(expected)
                || !valid_relative_path(&preimage.path)
                || preimage.operation.is_empty()
                || preimage.operation.len() > 100
                || preimage.before_bytes.is_some() != preimage.before_mode.is_some()
            {
                return Err(invalid("ChangeBatch rollback entry is invalid"));
            }
            preimage.after.validate()?;
            let before_len = preimage
                .before_bytes
                .as_ref()
                .map_or(0, Vec::len)
                .try_into()
                .map_err(|_| capacity("ChangeBatch preimage length is unsupported"))?;
            let before_digest = preimage.before_bytes.as_deref().map(digest_bytes);
            Ok((
                preimage.before_bytes.clone(),
                StoredRollbackEntry {
                    ordinal: preimage.ordinal,
                    path: preimage.path.clone(),
                    operation: preimage.operation.clone(),
                    before_exists: preimage.before_bytes.is_some(),
                    before_digest: before_digest.clone(),
                    before_len,
                    before_mode: preimage.before_mode.clone(),
                    after: StoredFileState::from(&preimage.after),
                    blob_digest: before_digest,
                },
            ))
        })
        .collect()
}

fn prepare_mutation_preimage_manifest(
    record: &PreparedPreimageJournalRecord,
) -> Result<StoredMutationPreimageManifest, ChangeBatchJournalError> {
    let mut total = 0_u64;
    let mut previous_path: Option<&str> = None;
    let files = record
        .files()
        .iter()
        .map(|file| {
            if !valid_relative_path(file.path())
                || previous_path.is_some_and(|path| path >= file.path())
                || file.bytes().is_some() != file.digest().is_some()
                || file.bytes().is_some() != file.mode().is_some()
            {
                return Err(invalid("ChangeBatch mutation preimage is invalid"));
            }
            previous_path = Some(file.path());
            let byte_length = u64::try_from(file.bytes().map_or(0, <[u8]>::len))
                .map_err(|_| capacity("ChangeBatch mutation preimage is too large"))?;
            total = total
                .checked_add(byte_length)
                .ok_or_else(|| capacity("ChangeBatch mutation preimage total overflowed"))?;
            if let (Some(bytes), Some(digest)) = (file.bytes(), file.digest())
                && digest_bytes(bytes) != *digest
            {
                return Err(invalid("ChangeBatch mutation preimage digest is invalid"));
            }
            Ok(StoredMutationPreimage {
                path: file.path().to_owned(),
                digest: file.digest().cloned(),
                mode: file.mode().map(str::to_owned),
                expected_after_digest: file.expected_after_digest().cloned(),
                expected_after_mode: file.expected_after_mode().map(str::to_owned),
                byte_length,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if total != record.total_preimage_bytes() {
        return Err(invalid(
            "ChangeBatch mutation preimage byte total is invalid",
        ));
    }
    Ok(StoredMutationPreimageManifest {
        batch_id: record.batch_id().clone(),
        plan_digest: record.plan_digest().clone(),
        preimage_digest: record.preimage_digest().clone(),
        total_preimage_bytes: record.total_preimage_bytes(),
        files,
    })
}

fn load_mutation_preimage_record(
    connection: &Connection,
    blob_root: &Path,
    execution: &ChangeBatchJournalRecord,
) -> Result<Option<PreparedPreimageJournalRecord>, ChangeBatchJournalError> {
    let Some(bytes) = connection
        .query_row(
            "SELECT manifest_json FROM change_batch_mutation_preimage_manifest
             WHERE batch_id = ?1",
            params![execution.event.identity.batch_id.0],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| unavailable("ChangeBatch mutation preimages cannot be read"))?
    else {
        return Ok(None);
    };
    let manifest: StoredMutationPreimageManifest = serde_json::from_slice(&bytes)
        .map_err(|_| corrupt("ChangeBatch mutation preimage manifest is corrupt"))?;
    if canonical_bytes(&manifest)? != bytes
        || manifest.batch_id != execution.event.identity.batch_id
        || manifest.plan_digest != execution.plan_digest
        || !valid_digest(&manifest.preimage_digest)
        || manifest.total_preimage_bytes > MAX_CHANGE_BATCH_PREIMAGE_BYTES
    {
        return Err(corrupt(
            "ChangeBatch mutation preimage manifest identity changed",
        ));
    }
    let plan = prepare_change_batch(&execution.event, ChangeBatchPolicy::default())
        .map_err(|_| corrupt("ChangeBatch mutation recovery plan is corrupt"))?;
    let files = manifest
        .files
        .into_iter()
        .map(|file| {
            let bytes = file
                .digest
                .as_ref()
                .map(|digest| {
                    let blob = fs::read(blob_path(blob_root, digest)?)
                        .map_err(|_| corrupt("ChangeBatch mutation preimage blob is missing"))?;
                    if u64::try_from(blob.len()).ok() != Some(file.byte_length)
                        || digest_bytes(&blob) != *digest
                    {
                        return Err(corrupt(
                            "ChangeBatch mutation preimage blob changed under its digest",
                        ));
                    }
                    Ok(blob)
                })
                .transpose()?;
            if bytes.is_none() && file.byte_length != 0 {
                return Err(corrupt(
                    "ChangeBatch absent mutation preimage has source bytes",
                ));
            }
            Ok(FilePreimage::from_persisted(
                file.path,
                bytes,
                file.digest,
                file.mode,
                file.expected_after_digest,
                file.expected_after_mode,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let record = rebuild_preimage_journal_record(
        &plan,
        manifest.preimage_digest,
        manifest.total_preimage_bytes,
        files,
    )
    .map_err(|_| corrupt("ChangeBatch mutation preimage manifest digest changed"))?;
    Ok(Some(record))
}

fn load_rollback_entries(
    connection: &Connection,
    batch_id: &str,
) -> Result<Vec<StoredRollbackEntry>, ChangeBatchJournalError> {
    let mut statement = connection
        .prepare(
            "SELECT entry_json FROM change_batch_rollback_entry
             WHERE batch_id = ?1 ORDER BY ordinal",
        )
        .map_err(|_| unavailable("ChangeBatch rollback manifest cannot be opened"))?;
    let rows = statement
        .query_map(params![batch_id], |row| row.get::<_, Vec<u8>>(0))
        .map_err(|_| unavailable("ChangeBatch rollback manifest cannot be read"))?;
    rows.map(|row| {
        let bytes = row.map_err(|_| unavailable("ChangeBatch rollback row cannot be read"))?;
        serde_json::from_slice(&bytes).map_err(|_| corrupt("ChangeBatch rollback entry is corrupt"))
    })
    .collect()
}

fn load_rollback_entry(
    connection: &Connection,
    batch_id: &str,
    ordinal: u64,
) -> Result<Option<StoredRollbackEntry>, ChangeBatchJournalError> {
    connection
        .query_row(
            "SELECT entry_json FROM change_batch_rollback_entry
             WHERE batch_id = ?1 AND ordinal = ?2",
            params![batch_id, i64_from_u64(ordinal)?],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| unavailable("ChangeBatch rollback entry cannot be read"))?
        .map(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|_| corrupt("ChangeBatch rollback entry is corrupt"))
        })
        .transpose()
}

fn load_authority(
    connection: &Connection,
    batch_id: &str,
) -> Result<Option<ChangeBatchIdentity>, ChangeBatchJournalError> {
    connection
        .query_row(
            "SELECT authority_json FROM change_batch_execution WHERE batch_id = ?1",
            params![batch_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| unavailable("ChangeBatch authority cannot be read"))?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|_| corrupt("ChangeBatch authority is corrupt"))
        })
        .transpose()
}

fn load_base_revision(
    connection: &Connection,
    batch_id: &str,
) -> Result<Option<WorkspaceRevision>, ChangeBatchJournalError> {
    connection
        .query_row(
            "SELECT base_revision FROM change_batch_execution WHERE batch_id = ?1",
            params![batch_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| unavailable("ChangeBatch sealed base cannot be read"))
        .map(|revision| revision.map(WorkspaceRevision))
}

fn load_progress_events(
    transaction: &Transaction<'_>,
    batch_id: &str,
) -> Result<Vec<ChangeBatchProgressEvent>, ChangeBatchJournalError> {
    let mut statement = transaction
        .prepare(
            "SELECT event_json FROM change_batch_progress
             WHERE batch_id = ?1 ORDER BY sequence",
        )
        .map_err(|_| unavailable("ChangeBatch progress ledger cannot be opened"))?;
    let rows = statement
        .query_map(params![batch_id], |row| row.get::<_, Vec<u8>>(0))
        .map_err(|_| unavailable("ChangeBatch progress ledger cannot be read"))?;
    rows.map(|row| {
        let bytes = row.map_err(|_| unavailable("ChangeBatch progress row cannot be read"))?;
        serde_json::from_slice(&bytes).map_err(|_| corrupt("ChangeBatch progress event is corrupt"))
    })
    .collect()
}

fn load_phase_and_operation(
    transaction: &Transaction<'_>,
    batch_id: &str,
) -> Result<Option<(ChangeBatchExecutionPhase, u64)>, ChangeBatchJournalError> {
    transaction
        .query_row(
            "SELECT phase, next_operation FROM change_batch_execution WHERE batch_id = ?1",
            params![batch_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|_| unavailable("ChangeBatch execution cursor cannot be read"))?
        .map(|(phase, operation)| {
            Ok((
                ChangeBatchExecutionPhase::parse(&phase)?,
                u64::try_from(operation)
                    .map_err(|_| corrupt("ChangeBatch operation cursor is invalid"))?,
            ))
        })
        .transpose()
}

fn update_phase(
    transaction: &Transaction<'_>,
    batch_id: &str,
    expected: ChangeBatchExecutionPhase,
    next: ChangeBatchExecutionPhase,
    now: &str,
) -> Result<(), ChangeBatchJournalError> {
    let changed = transaction
        .execute(
            "UPDATE change_batch_execution SET phase = ?3, updated_at = ?4
             WHERE batch_id = ?1 AND phase = ?2",
            params![batch_id, expected.as_str(), next.as_str(), now],
        )
        .map_err(|_| unavailable("ChangeBatch phase cannot be persisted"))?;
    if changed != 1 {
        return Err(conflict("ChangeBatch phase compare-and-set failed"));
    }
    Ok(())
}

const fn legal_phase_transition(
    from: ChangeBatchExecutionPhase,
    to: ChangeBatchExecutionPhase,
) -> bool {
    matches!(
        (from, to),
        (
            ChangeBatchExecutionPhase::PreimagesReady,
            ChangeBatchExecutionPhase::Applying
        ) | (
            ChangeBatchExecutionPhase::Applying,
            ChangeBatchExecutionPhase::Applied
                | ChangeBatchExecutionPhase::RollbackRequired
                | ChangeBatchExecutionPhase::StateUncertain
        ) | (
            ChangeBatchExecutionPhase::RollbackRequired,
            ChangeBatchExecutionPhase::RolledBack | ChangeBatchExecutionPhase::StateUncertain
        )
    )
}

fn validate_event(event: &ChangeBatchProposalEvent) -> Result<(), ChangeBatchJournalError> {
    validate_change_batch_identity_derivation(&event.identity)
        .map_err(|_| invalid("ChangeBatch identity derivation is invalid"))?;
    let expected = digest_bytes(event.proposal.patch.as_bytes());
    if expected != event.identity.patch_digest
        || !valid_workspace_revision(&event.identity.workspace_revision)
    {
        return Err(invalid(
            "ChangeBatch proposal digest or base revision is invalid",
        ));
    }
    let bytes = canonical_bytes(event)?;
    let decoded: ChangeBatchProposalEvent = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("ChangeBatch proposal event is non-canonical"))?;
    if decoded != *event {
        return Err(invalid(
            "ChangeBatch proposal event changed during validation",
        ));
    }
    Ok(())
}

fn validate_progress_contract(
    event: &ChangeBatchProgressEvent,
    bytes: &[u8],
) -> Result<(), ChangeBatchJournalError> {
    let decoded: ChangeBatchProgressEvent = serde_json::from_slice(bytes)
        .map_err(|_| invalid("ChangeBatch progress event is non-canonical"))?;
    if decoded != *event {
        return Err(invalid(
            "ChangeBatch progress event changed during validation",
        ));
    }
    Ok(())
}

fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, ChangeBatchJournalError> {
    serde_json::to_vec(value).map_err(|_| invalid("ChangeBatch value cannot be encoded"))
}

fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
    Sha256Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn valid_digest(digest: &Sha256Digest) -> bool {
    digest.0.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

fn valid_workspace_revision(value: &WorkspaceRevision) -> bool {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| serde_json::from_value::<WorkspaceRevision>(value).ok())
        .as_ref()
        == Some(value)
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4_096
        && Path::new(value)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn blob_path(root: &Path, digest: &Sha256Digest) -> Result<PathBuf, ChangeBatchJournalError> {
    if !valid_digest(digest) {
        return Err(invalid("ChangeBatch preimage digest is invalid"));
    }
    Ok(root.join(
        digest
            .0
            .strip_prefix("sha256:")
            .ok_or_else(|| invalid("ChangeBatch preimage digest is invalid"))?,
    ))
}

fn ensure_private_directory(path: &Path) -> Result<(), ChangeBatchJournalError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| unavailable("ChangeBatch private directory cannot be inspected"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid("ChangeBatch private directory is invalid"));
        }
    } else {
        fs::create_dir_all(path)
            .map_err(|_| unavailable("ChangeBatch private directory cannot be created"))?;
        sync_directory(
            path.parent()
                .ok_or_else(|| invalid("ChangeBatch private directory has no parent"))?,
        )?;
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| unavailable("ChangeBatch private directory permissions cannot be set"))?;
    Ok(())
}

fn ensure_private_file(path: &Path) -> Result<(), ChangeBatchJournalError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| unavailable("ChangeBatch database cannot be inspected"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(invalid("ChangeBatch database path is invalid"));
        }
    } else {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(path)
            .map_err(|_| unavailable("ChangeBatch database cannot be created"))?;
        file.sync_all()
            .map_err(|_| unavailable("ChangeBatch database cannot be synchronized"))?;
        sync_directory(
            path.parent()
                .ok_or_else(|| invalid("ChangeBatch database has no parent"))?,
        )?;
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| unavailable("ChangeBatch database permissions cannot be set"))?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), ChangeBatchJournalError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| unavailable("ChangeBatch private directory cannot be synchronized"))
}

fn i64_from_u64(value: u64) -> Result<i64, ChangeBatchJournalError> {
    i64::try_from(value).map_err(|_| invalid("ChangeBatch operation index is unsupported"))
}

const fn invalid(message: &'static str) -> ChangeBatchJournalError {
    ChangeBatchJournalError::new(ChangeBatchJournalErrorCode::Invalid, message)
}

const fn conflict(message: &'static str) -> ChangeBatchJournalError {
    ChangeBatchJournalError::new(ChangeBatchJournalErrorCode::Conflict, message)
}

const fn corrupt(message: &'static str) -> ChangeBatchJournalError {
    ChangeBatchJournalError::new(ChangeBatchJournalErrorCode::Corrupt, message)
}

const fn capacity(message: &'static str) -> ChangeBatchJournalError {
    ChangeBatchJournalError::new(ChangeBatchJournalErrorCode::Capacity, message)
}

const fn unavailable(message: &'static str) -> ChangeBatchJournalError {
    ChangeBatchJournalError::new(ChangeBatchJournalErrorCode::Unavailable, message)
}

#[cfg(test)]
mod tests {
    use winwincode_domain::WorkspaceRevision;
    use winwincode_execution_port::{
        diagnostic_parser::{DiagnosticParseBatch, build_diagnostic_baseline},
        generated::DiagnosticParserVersion,
    };

    use super::{ValidationDiagnosticEvaluation, validate_diagnostic_evaluation};

    #[test]
    fn existing_baseline_allows_a_durable_result_parser_failure() {
        let base_revision = WorkspaceRevision(format!("git-tree:{}", "a".repeat(40)));
        let result_revision = WorkspaceRevision(format!("git-tree:{}", "b".repeat(40)));
        let baseline = build_diagnostic_baseline(
            base_revision.clone(),
            &[DiagnosticParseBatch {
                parser_version: DiagnosticParserVersion::TypescriptV1,
                diagnostics: Vec::new(),
            }],
        )
        .expect("canonical baseline");
        validate_diagnostic_evaluation(&ValidationDiagnosticEvaluation {
            base_revision,
            result_revision,
            baseline: Some(baseline),
            result: None,
            comparison: None,
            parser_failed: true,
            disposition: "repair_required".to_owned(),
            reason_code: Some("diagnostic.parser_error".to_owned()),
        })
        .expect("parser failure remains durable beside an existing baseline");
    }
}
