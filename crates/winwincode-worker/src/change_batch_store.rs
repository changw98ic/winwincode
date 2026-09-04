// SPDX-License-Identifier: Apache-2.0

//! Durable Worker-owned state for one delegated `ChangeBatch`.
//!
//! This store is the Worker's own evidence root for the delegated loop. Every
//! observation intent, Provider exchange frame, validation decision, progress
//! fact, and receipt is committed here before it becomes observable, and every
//! write is idempotent so an exact replay after a process loss can never mint a
//! second effect. Two invariants are structural:
//!
//! * An Observer chunk is retained only contiguously (`Inserted`), and an exact
//!   replay of a retained frame is a `Duplicate` while a missing predecessor is
//!   a `Gap` that never advances the confirmed cursor.
//! * The workspace accepted revision only advances through
//!   [`ChangeBatchStore::accept_observed_checkpoint`], which releases the active
//!   batch exactly once and rejects stale or foreign facts with
//!   [`ObservationGateResult::Stale`] instead of an error.

use std::{
    collections::HashSet,
    fmt, fs,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest as _, Sha256};
use winwincode_domain::{
    ChangeBatchId, ExecutionJobId, Instant, ModelExchangeId, Sha256Digest, WorkspaceRevision,
};
use winwincode_execution_port::change_batch_identity::validate_change_batch_identity_derivation;
use winwincode_execution_port::change_batch_progress::ChangeBatchProgressLedger;
use winwincode_execution_port::generated::{
    AppliedFileOperation, AppliedFileSummary, ChangeBatchIdentity, ChangeBatchProgressEvent,
    ChangeBatchProgressState, ChangeBatchProposalEvent, ChangeBatchReceipt,
    ChangeBatchReceiptStatus, ExecutionPortMessage, ModelOpenMessage, ObservationReceipt,
    ObservationRequest,
};
use winwincode_execution_port::observation_contract::{
    MAX_OBSERVATION_RESPONSE_BYTES, validate_observation_receipt, validate_observation_request,
};
use winwincode_execution_port::typed_replay::stream_key_from_message;

/// Durable contiguous Observer model stream retention result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationChunkRetention {
    Inserted { confirmed_sequence: i64 },
    Duplicate { confirmed_sequence: i64 },
    Gap { confirmed_sequence: i64 },
}

/// One bounded Provider frame submitted for contiguous retention.
///
/// Only the strict response text, terminal usage, and stable terminal kind are
/// retained. Provider reasoning, tool data, and raw envelopes never enter the
/// store.
pub struct ObservationModelFrame<'frame> {
    pub model_exchange_id: ModelExchangeId,
    pub sequence: i64,
    pub chunk_digest: Sha256Digest,
    pub response_delta: &'frame [u8],
    pub model_usage: Option<winwincode_execution_port::generated::ExecutionOutcomeUsage>,
    pub terminal_status: Option<&'frame str>,
}

/// Result of a revision-gated observation transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationGateResult {
    Accepted,
    Stale,
}

/// Whether one durable write inserted new state or replayed existing state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreRetention {
    Inserted,
    Replay,
}

/// Revalidated durable one-shot Provider exchange state for one observation.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservationModelRecord {
    pub request: ObservationRequest,
    pub model_open: Option<ModelOpenMessage>,
    pub confirmed_sequence: i64,
    pub response_bytes: Vec<u8>,
    pub model_usage: Option<winwincode_execution_port::generated::ExecutionOutcomeUsage>,
    pub terminal_status: Option<String>,
    pub receipt: Option<ObservationReceipt>,
}

/// Durable deterministic decision over one exact base/result diagnostic pair.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ValidationDiagnosticEvaluation {
    pub base_revision: WorkspaceRevision,
    pub result_revision: WorkspaceRevision,
    pub baseline: Option<winwincode_execution_port::generated::DiagnosticBaseline>,
    pub result: Option<winwincode_execution_port::generated::DiagnosticBaseline>,
    pub comparison: Option<winwincode_execution_port::generated::DiagnosticBaselineComparison>,
    pub parser_failed: bool,
    pub disposition: String,
    pub reason_code: Option<String>,
}

/// Revalidated durable one-proposal state: intent, receipt, and base revision.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangeBatchRecord {
    pub event: ChangeBatchProposalEvent,
    pub base_revision: WorkspaceRevision,
    pub plan_digest: Sha256Digest,
    pub receipt: Option<ChangeBatchReceipt>,
}

/// Durable accepted-revision anchor and active batch for one workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceBatchBinding {
    pub workspace_id: String,
    pub accepted_revision: WorkspaceRevision,
    pub active_batch_id: Option<ChangeBatchId>,
    pub state: BatchState,
    pub checkpoint_revision: Option<WorkspaceRevision>,
    pub checkpoint_delta_digest: Option<Sha256Digest>,
}

/// Bounded durable Worker state for the workspace that owns one proposal.
///
/// The PR1 Worker never writes files itself: it retains evidence, routes one
/// bounded observation, and gates the accepted revision. Mutations beyond this
/// state machine belong to a later delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchState {
    Idle,
    Applying,
    ValidationPending,
    ObservationPending,
    Accepted,
    RepairRequired,
    Quarantined,
}

impl BatchState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Applying => "applying",
            Self::ValidationPending => "validation_pending",
            Self::ObservationPending => "observation_pending",
            Self::Accepted => "accepted",
            Self::RepairRequired => "repair_required",
            Self::Quarantined => "quarantined",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "idle" => Some(Self::Idle),
            "applying" => Some(Self::Applying),
            "validation_pending" => Some(Self::ValidationPending),
            "observation_pending" => Some(Self::ObservationPending),
            "accepted" => Some(Self::Accepted),
            "repair_required" => Some(Self::RepairRequired),
            "quarantined" => Some(Self::Quarantined),
            _ => None,
        }
    }
}

/// Secret-free durable store failure. Source bytes never appear in a message.
#[derive(Debug)]
pub struct ChangeBatchStoreError {
    message: &'static str,
}

impl ChangeBatchStoreError {
    fn new(message: &'static str) -> Self {
        Self { message }
    }

    /// Returns the stable secret-safe failure description.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ChangeBatchStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ChangeBatchStoreError {}

const DATABASE_DIRECTORY: &str = ".workspaces-change-batches";
const DATABASE_FILE: &str = "change-batch.sqlite3";
const MAX_RETAINED_RECORDS: usize = 1024;
const MAX_SEQUENCE: i64 = 9_007_199_254_740_991;
const MAX_DELTA_PATH_BYTES: usize = 4096;
const DELTA_DIGEST_DOMAIN: &[u8] = b"winwincode.change-batch-delta.v1\0";

fn invalid(message: &'static str) -> ChangeBatchStoreError {
    ChangeBatchStoreError::new(message)
}

fn corrupt() -> ChangeBatchStoreError {
    ChangeBatchStoreError::new("ChangeBatch durable state is corrupt")
}

fn conflict() -> ChangeBatchStoreError {
    ChangeBatchStoreError::new("ChangeBatch durable state changed on replay")
}

fn unavailable() -> ChangeBatchStoreError {
    ChangeBatchStoreError::new("ChangeBatch durable state is unavailable")
}

/// Canonical wire bytes: a serialize/deserialize round trip rejects any shape
/// that cannot be read back exactly, so stored evidence is always replayable.
fn canonical_bytes<T: serde::Serialize + serde::de::DeserializeOwned>(
    value: &T,
) -> Result<Vec<u8>, ChangeBatchStoreError> {
    let bytes = serde_json::to_vec(value).map_err(|_| corrupt())?;
    let canonical: T = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
    serde_json::to_vec(&canonical).map_err(|_| corrupt())
}

fn valid_digest(digest: &Sha256Digest) -> bool {
    let Some(encoded) = digest.0.strip_prefix("sha256:") else {
        return false;
    };
    encoded.len() == 64 && encoded.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// A batch id is itself the canonical `sha256:` digest of the framed proposal
/// content, so it must be accepted only in that exact form. It is never
/// re-hashed here: re-deriving it is the separate
/// `validate_change_batch_identity_derivation` check.
fn valid_batch_id(batch_id: &ChangeBatchId) -> bool {
    digest_str_is_canonical(&batch_id.0)
}

fn digest_str_is_canonical(encoded: &str) -> bool {
    let Some(hex) = encoded.strip_prefix("sha256:") else {
        return false;
    };
    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_revision(revision: &WorkspaceRevision) -> bool {
    let Some(encoded) = revision.0.strip_prefix("git-tree:") else {
        return false;
    };
    (encoded.len() == 40 || encoded.len() == 64)
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_progress_sequence(sequence: i64) -> bool {
    (1..=MAX_SEQUENCE).contains(&sequence)
}

fn valid_progress_event(event: &ChangeBatchProgressEvent) -> bool {
    valid_batch_id(&event.identity.batch_id)
        && valid_progress_sequence(event.sequence)
        && !event.occurred_at.0.is_empty()
        && !event.summary.is_empty()
}

/// Worker-owned `SQLite` store for delegated `ChangeBatch` evidence.
pub struct ChangeBatchStore {
    connection: Connection,
    database: PathBuf,
}

impl fmt::Debug for ChangeBatchStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChangeBatchStore")
            .finish_non_exhaustive()
    }
}

impl ChangeBatchStore {
    /// Opens the private store with FULL synchronous WAL durability.
    ///
    /// # Errors
    ///
    /// Rejects unavailable state paths and unusable database state.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ChangeBatchStoreError> {
        let root = root.as_ref();
        let directory = root.join(DATABASE_DIRECTORY);
        ensure_private_directory(&directory)?;
        let database = directory.join(DATABASE_FILE);
        ensure_private_file(&database)?;
        let connection = Connection::open(&database).map_err(|_| unavailable())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;",
            )
            .map_err(|_| unavailable())?;
        connection.execute_batch(SCHEMA).map_err(|_| corrupt())?;
        Ok(Self {
            connection,
            database,
        })
    }

    /// Returns the durable database path for restart diagnostics.
    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database
    }

    /// Creates or replays the workspace durable accepted-revision anchor.
    ///
    /// # Errors
    ///
    /// Rejects an invalid revision or a changed initial revision.
    pub fn retain_workspace_binding(
        &mut self,
        workspace_id: &str,
        accepted_revision: &WorkspaceRevision,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        if workspace_id.is_empty() || !valid_revision(accepted_revision) || now.0.is_empty() {
            return Err(invalid("ChangeBatch workspace anchor is invalid"));
        }
        let existing = self.workspace_binding(workspace_id)?;
        match existing {
            Some(binding) if binding.accepted_revision == *accepted_revision => {
                Ok(StoreRetention::Replay)
            }
            Some(_) => Err(conflict()),
            None => {
                self.connection
                    .execute(
                        "INSERT INTO change_batch_workspace
                           (workspace_id, accepted_revision, active_batch_id, state,
                            checkpoint_revision, checkpoint_delta_digest, updated_at)
                         VALUES (?1, ?2, NULL, 'idle', NULL, NULL, ?3)",
                        params![workspace_id, accepted_revision.0, now.0],
                    )
                    .map_err(|_| unavailable())?;
                Ok(StoreRetention::Inserted)
            }
        }
    }

    /// Loads one workspace binding.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored state.
    pub fn workspace_binding(
        &self,
        workspace_id: &str,
    ) -> Result<Option<WorkspaceBatchBinding>, ChangeBatchStoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT accepted_revision, active_batch_id, state, checkpoint_revision,
                        checkpoint_delta_digest
                 FROM change_batch_workspace WHERE workspace_id = ?1",
                params![workspace_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?;
        let Some((
            accepted_revision,
            active_batch_id,
            state,
            checkpoint_revision,
            checkpoint_delta_digest,
        )) = row
        else {
            return Ok(None);
        };
        let Some(state) = BatchState::parse(&state) else {
            return Err(corrupt());
        };
        Ok(Some(WorkspaceBatchBinding {
            workspace_id: workspace_id.to_owned(),
            accepted_revision: WorkspaceRevision(accepted_revision),
            active_batch_id: active_batch_id.map(ChangeBatchId),
            state,
            checkpoint_revision: checkpoint_revision.map(WorkspaceRevision),
            checkpoint_delta_digest: checkpoint_delta_digest.map(Sha256Digest),
        }))
    }

    /// Resolves the workspace that owns one batch.
    ///
    /// # Errors
    ///
    /// Rejects unavailable state.
    pub fn workspace_id_for_batch(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Option<String>, ChangeBatchStoreError> {
        self.connection
            .query_row(
                "SELECT workspace_id FROM change_batch_intent WHERE batch_id = ?1",
                params![batch_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| unavailable())
    }

    /// Atomically retains one intent and claims the workspace for it.
    ///
    /// # Errors
    ///
    /// Rejects changed replay bytes, a stale base revision, or a second active
    /// batch on the same workspace.
    pub fn retain_claimed_intent(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProposalEvent,
        expected_base_revision: &WorkspaceRevision,
        plan_digest: &Sha256Digest,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        if !valid_digest(plan_digest) || !valid_revision(expected_base_revision) || now.0.is_empty()
        {
            return Err(invalid("ChangeBatch intent is invalid"));
        }
        // A proposal is retained only when its batch id is the canonical
        // derivation of the framed proposal content it carries.
        validate_change_batch_identity_derivation(&event.identity)
            .map_err(|_| invalid("ChangeBatch identity is not canonically derived"))?;
        let event_bytes = canonical_bytes(event)?;
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let existing = transaction
            .query_row(
                "SELECT event_json, base_revision, plan_digest FROM change_batch_intent
                 WHERE batch_id = ?1",
                params![event.identity.batch_id.0],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?;
        let retention = if let Some((stored_event, stored_base, stored_plan)) = existing {
            if stored_event != event_bytes
                || WorkspaceRevision(stored_base) != *expected_base_revision
                || Sha256Digest(stored_plan) != *plan_digest
            {
                return Err(conflict());
            }
            StoreRetention::Replay
        } else {
            transaction
                .execute(
                    "INSERT INTO change_batch_intent
                           (batch_id, job_id, workspace_id, event_json, base_revision,
                            plan_digest, receipt_json, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?7)",
                    params![
                        event.identity.batch_id.0,
                        event.identity.job_id.0,
                        workspace_id,
                        event_bytes,
                        expected_base_revision.0,
                        plan_digest.0,
                        now.0,
                    ],
                )
                .map_err(|_| unavailable())?;
            StoreRetention::Inserted
        };
        mint_workspace_anchor(&transaction, workspace_id, expected_base_revision, now)?;
        let binding = load_binding(&transaction, workspace_id)?;
        if binding.accepted_revision != *expected_base_revision {
            return Err(conflict());
        }
        if binding.active_batch_id.as_ref() == Some(&event.identity.batch_id) {
            transaction.commit().map_err(|_| unavailable())?;
            return Ok(retention);
        }
        if binding.active_batch_id.is_some()
            || !matches!(binding.state, BatchState::Idle | BatchState::Accepted)
        {
            return Err(conflict());
        }
        let changed = transaction
            .execute(
                "UPDATE change_batch_workspace
                 SET active_batch_id = ?2, state = 'applying', checkpoint_revision = NULL,
                     checkpoint_delta_digest = NULL, updated_at = ?3
                 WHERE workspace_id = ?1 AND state IN ('idle', 'accepted')
                   AND active_batch_id IS NULL",
                params![workspace_id, event.identity.batch_id.0, now.0],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err(conflict());
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(retention)
    }

    /// Loads one proposal record with its durable receipt.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored state.
    pub fn batch_record(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Option<ChangeBatchRecord>, ChangeBatchStoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT event_json, base_revision, plan_digest, receipt_json
                 FROM change_batch_intent WHERE batch_id = ?1",
                params![batch_id.0],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?;
        let Some((event_bytes, base_revision, plan_digest, receipt_bytes)) = row else {
            return Ok(None);
        };
        Ok(Some(ChangeBatchRecord {
            event: decode(&event_bytes)?,
            base_revision: WorkspaceRevision(base_revision),
            plan_digest: Sha256Digest(plan_digest),
            receipt: receipt_bytes.map(|bytes| decode(&bytes)).transpose()?,
        }))
    }

    /// Loads every retained proposal record for one Job.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored state or an unbounded record backlog.
    pub fn records_for_job(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Vec<ChangeBatchRecord>, ChangeBatchStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT batch_id FROM change_batch_intent WHERE job_id = ?1
                 ORDER BY created_at, batch_id",
            )
            .map_err(|_| unavailable())?;
        let batch_ids = statement
            .query_map(params![job_id.0], |row| row.get::<_, String>(0))
            .map_err(|_| unavailable())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| unavailable())?;
        if batch_ids.len() > MAX_RETAINED_RECORDS {
            return Err(ChangeBatchStoreError::new(
                "ChangeBatch record backlog is too large",
            ));
        }
        batch_ids
            .iter()
            .map(|batch_id| {
                self.batch_record(&ChangeBatchId(batch_id.clone()))
                    .and_then(|record| record.ok_or_else(corrupt))
            })
            .collect()
    }

    /// Appends one progress fact that does not move the workspace state.
    ///
    /// # Errors
    ///
    /// Rejects changed replay bytes or a duplicated sequence.
    pub fn append_progress(
        &mut self,
        event: &ChangeBatchProgressEvent,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        let bytes = canonical_bytes(event)?;
        require_valid_progress(event)?;
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let retention = insert_progress(&transaction, event, &bytes)?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(retention)
    }

    /// Retains one progress fact that moves the workspace between two states.
    ///
    /// # Errors
    ///
    /// Rejects stale workspace state, foreign batches, or changed replay bytes.
    pub fn retain_workspace_progress(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProgressEvent,
        expected: BatchState,
        next: BatchState,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        require_valid_progress(event)?;
        let bytes = canonical_bytes(event)?;
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let binding = load_binding(&transaction, workspace_id)?;
        let batch_id = &event.identity.batch_id;
        if let Some(existing) = load_progress_bytes(&transaction, batch_id, event.sequence)? {
            if existing == bytes
                && binding.active_batch_id.as_ref() == Some(batch_id)
                && binding.state == next
            {
                transaction.commit().map_err(|_| unavailable())?;
                return Ok(StoreRetention::Replay);
            }
            return Err(conflict());
        }
        if binding.active_batch_id.as_ref() != Some(batch_id) || binding.state != expected {
            return Err(conflict());
        }
        let changed = transaction
            .execute(
                "UPDATE change_batch_workspace SET state = ?4, updated_at = ?5
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = ?3",
                params![
                    workspace_id,
                    batch_id.0,
                    expected.as_str(),
                    next.as_str(),
                    event.occurred_at.0
                ],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err(conflict());
        }
        insert_progress(&transaction, event, &bytes)?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(StoreRetention::Inserted)
    }

    /// Retains the exact apply checkpoint and its applied receipt.
    ///
    /// # Errors
    ///
    /// Rejects a foreign batch, stale state, or a receipt that does not bind
    /// the retained identity and base revision.
    pub fn retain_applied_checkpoint(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProgressEvent,
        receipt: &ChangeBatchReceipt,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        self.retain_checkpoint_receipt(
            workspace_id,
            event,
            receipt,
            BatchState::Applying,
            BatchState::ValidationPending,
            now,
        )
    }

    /// Retains one validated checkpoint with its exact validation receipt.
    ///
    /// # Errors
    ///
    /// Rejects a foreign batch, stale state, or changed replay bytes.
    pub fn retain_validated_checkpoint(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProgressEvent,
        receipt: &ChangeBatchReceipt,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        self.retain_checkpoint_receipt(
            workspace_id,
            event,
            receipt,
            BatchState::ValidationPending,
            BatchState::ValidationPending,
            now,
        )
    }

    fn retain_checkpoint_receipt(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProgressEvent,
        receipt: &ChangeBatchReceipt,
        expected: BatchState,
        next: BatchState,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        require_valid_progress(event)?;
        if now.0.is_empty() {
            return Err(invalid("ChangeBatch checkpoint is invalid"));
        }
        let progress_bytes = canonical_bytes(event)?;
        let receipt_bytes = canonical_bytes(receipt)?;
        let batch_id = event.identity.batch_id.clone();
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let record = load_record(&transaction, &batch_id)?;
        if record.event.identity != event.identity {
            return Err(conflict());
        }
        validate_receipt(receipt, &record.event.identity, &record.base_revision)?;
        let binding = load_binding(&transaction, workspace_id)?;
        if let Some(existing) = record.receipt.as_ref() {
            if existing != receipt {
                return Err(conflict());
            }
            // The proven checkpoint is already durable, so only its progress
            // fact is retained here.
            if load_progress_bytes(&transaction, &batch_id, event.sequence)?.is_some() {
                if load_progress_bytes(&transaction, &batch_id, event.sequence)?.as_deref()
                    != Some(progress_bytes.as_slice())
                    || binding.active_batch_id.as_ref() != Some(&batch_id)
                    || binding.state != next
                {
                    return Err(conflict());
                }
                transaction.commit().map_err(|_| unavailable())?;
                return Ok(StoreRetention::Replay);
            }
            if binding.active_batch_id.as_ref() != Some(&batch_id) || binding.state != next {
                return Err(conflict());
            }
            insert_progress(&transaction, event, &progress_bytes)?;
            transaction.commit().map_err(|_| unavailable())?;
            return Ok(StoreRetention::Inserted);
        }
        if binding.active_batch_id.as_ref() != Some(&batch_id) || binding.state != expected {
            return Err(conflict());
        }
        let checkpoint_revision = receipt
            .result_revision
            .clone()
            .ok_or_else(|| invalid("ChangeBatch checkpoint has no result revision"))?;
        let checkpoint_delta = receipt
            .delta_digest
            .clone()
            .ok_or_else(|| invalid("ChangeBatch checkpoint has no exact delta"))?;
        if !valid_revision(&checkpoint_revision) || !valid_digest(&checkpoint_delta) {
            return Err(invalid("ChangeBatch checkpoint identity is invalid"));
        }
        let changed = transaction
            .execute(
                "UPDATE change_batch_workspace
                 SET state = ?4, checkpoint_revision = ?5, checkpoint_delta_digest = ?6,
                     updated_at = ?7
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = ?3",
                params![
                    workspace_id,
                    batch_id.0,
                    expected.as_str(),
                    next.as_str(),
                    checkpoint_revision.0,
                    checkpoint_delta.0,
                    now.0,
                ],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err(conflict());
        }
        transaction
            .execute(
                "UPDATE change_batch_intent SET receipt_json = ?2, updated_at = ?3
                 WHERE batch_id = ?1 AND receipt_json IS NULL",
                params![batch_id.0, receipt_bytes, now.0],
            )
            .map_err(|_| unavailable())?;
        insert_progress(&transaction, event, &progress_bytes)?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(StoreRetention::Inserted)
    }

    /// Loads the ordered progress ledger of one batch.
    ///
    /// # Errors
    ///
    /// Rejects corrupt or out-of-order stored events.
    pub fn progress_events(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Vec<ChangeBatchProgressEvent>, ChangeBatchStoreError> {
        let events = stored_progress(&self.connection, batch_id)?;
        let mut ledger = ChangeBatchProgressLedger::new();
        for event in &events {
            ledger.record(event).map_err(|_| corrupt())?;
        }
        Ok(events)
    }

    /// Retains one terminal progress fact together with its final receipt.
    ///
    /// # Errors
    ///
    /// Rejects stale state, foreign batches, or changed replay bytes.
    pub fn retain_terminal_workspace_receipt(
        &mut self,
        workspace_id: &str,
        event: &ChangeBatchProgressEvent,
        receipt: &ChangeBatchReceipt,
        expected: BatchState,
        next: BatchState,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        self.retain_checkpoint_receipt(workspace_id, event, receipt, expected, next, now)
    }

    /// Retains one validation receipt against the retained proposal.
    ///
    /// # Errors
    ///
    /// Rejects an unknown batch, a stale result revision, or changed replay.
    pub fn retain_validation_receipt(
        &mut self,
        batch_id: &ChangeBatchId,
        receipt: &winwincode_execution_port::generated::ValidationReceipt,
        result_revision: &WorkspaceRevision,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        if now.0.is_empty() || !valid_revision(result_revision) {
            return Err(invalid("ChangeBatch validation receipt is invalid"));
        }
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let record = load_record(&transaction, batch_id)?;
        let binding = load_binding(&transaction, &record.workspace_id)?;
        let Some(stored) = record.receipt.as_ref() else {
            return Err(conflict());
        };
        if stored.result_revision.as_ref() != Some(result_revision) {
            return Err(conflict());
        }
        let mut upgraded = stored.clone();
        if upgraded.validation.as_ref() == Some(receipt) {
            transaction.commit().map_err(|_| unavailable())?;
            return Ok(StoreRetention::Replay);
        }
        if upgraded.validation.is_some() {
            return Err(conflict());
        }
        upgraded.validation = Some(receipt.clone());
        validate_receipt(&upgraded, &record.event.identity, &record.base_revision)?;
        let bytes = canonical_bytes(&upgraded)?;
        let changed = transaction
            .execute(
                "UPDATE change_batch_intent SET receipt_json = ?2, updated_at = ?3
                 WHERE batch_id = ?1",
                params![batch_id.0, bytes, now.0],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err(conflict());
        }
        if binding.state == BatchState::ValidationPending
            || binding.state == BatchState::ObservationPending
        {
            transaction
                .execute(
                    "UPDATE change_batch_workspace SET updated_at = ?2
                     WHERE workspace_id = ?1",
                    params![record.workspace_id, now.0],
                )
                .map_err(|_| unavailable())?;
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(StoreRetention::Inserted)
    }

    /// Loads the exact accepted diagnostic baseline for one batch.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored state.
    pub fn diagnostic_baseline(
        &self,
        batch_id: &ChangeBatchId,
        base_revision: &WorkspaceRevision,
    ) -> Result<
        Option<winwincode_execution_port::generated::DiagnosticBaseline>,
        ChangeBatchStoreError,
    > {
        let row = self
            .connection
            .query_row(
                "SELECT baseline_json, base_revision FROM change_batch_diagnostic
                 WHERE batch_id = ?1",
                params![batch_id.0],
                |row| Ok((row.get::<_, Option<Vec<u8>>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable())?;
        match row {
            None => Ok(None),
            Some((bytes, stored_base)) => {
                if WorkspaceRevision(stored_base) != *base_revision {
                    return Err(conflict());
                }
                bytes.map(|bytes| decode(&bytes)).transpose()
            }
        }
    }

    /// Retains one durable diagnostic decision for one batch.
    ///
    /// # Errors
    ///
    /// Rejects changed replay bytes or a changed base revision.
    pub fn retain_diagnostic_evaluation(
        &mut self,
        batch_id: &ChangeBatchId,
        evaluation: &ValidationDiagnosticEvaluation,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        if now.0.is_empty()
            || !valid_revision(&evaluation.base_revision)
            || !valid_revision(&evaluation.result_revision)
        {
            return Err(invalid("ChangeBatch diagnostic evaluation is invalid"));
        }
        let bytes = canonical_bytes(evaluation)?;
        let changed = self
            .connection
            .execute(
                "INSERT INTO change_batch_diagnostic
                   (batch_id, base_revision, result_revision, baseline_json, evaluation_json,
                    updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(batch_id) DO NOTHING",
                params![
                    batch_id.0,
                    evaluation.base_revision.0,
                    evaluation.result_revision.0,
                    evaluation
                        .baseline
                        .as_ref()
                        .map(canonical_bytes)
                        .transpose()?,
                    bytes,
                    now.0,
                ],
            )
            .map_err(|_| unavailable())?;
        if changed == 1 {
            return Ok(StoreRetention::Inserted);
        }
        let existing = self.diagnostic_evaluation(batch_id)?;
        match existing {
            Some(stored) if &stored == evaluation => Ok(StoreRetention::Replay),
            Some(_) => Err(conflict()),
            None => Err(corrupt()),
        }
    }

    /// Loads one durable diagnostic decision.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored state.
    pub fn diagnostic_evaluation(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Option<ValidationDiagnosticEvaluation>, ChangeBatchStoreError> {
        let bytes = self
            .connection
            .query_row(
                "SELECT evaluation_json FROM change_batch_diagnostic WHERE batch_id = ?1",
                params![batch_id.0],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(|_| unavailable())?
            .flatten();
        bytes.map(|bytes| decode(&bytes)).transpose()
    }

    /// Atomically retains one exact one-shot Observer intent.
    ///
    /// # Errors
    ///
    /// Rejects an invalid request, a stale checkpoint, or changed replay bytes.
    pub fn retain_observation_request(
        &mut self,
        workspace_id: &str,
        progress: &ChangeBatchProgressEvent,
        request: &ObservationRequest,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        validate_observation_request(request)
            .map_err(|_| invalid("ChangeBatch Observer request is invalid"))?;
        if progress.state != ChangeBatchProgressState::ObservationRequested
            || progress.identity != request.intent.identity
            || now.0.is_empty()
        {
            return Err(invalid("ChangeBatch Observer progress is invalid"));
        }
        let progress_bytes = canonical_bytes(progress)?;
        let request_bytes = canonical_bytes(request)?;
        let request_digest = digest_bytes(&request_bytes);
        let intent = &request.intent;
        let batch_id = intent.identity.batch_id.clone();
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let record = load_record(&transaction, &batch_id)?;
        if record.event.identity != intent.identity {
            return Err(conflict());
        }
        let binding = load_binding(&transaction, workspace_id)?;
        let existing = transaction
            .query_row(
                "SELECT request_json, request_digest FROM change_batch_observation
                 WHERE batch_id = ?1",
                params![batch_id.0],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|_| unavailable())?;
        if let Some((existing_request, existing_digest)) = existing {
            if existing_request == request_bytes
                && Sha256Digest(existing_digest) == request_digest
                && binding.state == BatchState::ObservationPending
                && binding.active_batch_id.as_ref() == Some(&batch_id)
                && load_progress_bytes(&transaction, &batch_id, progress.sequence)?.as_deref()
                    == Some(progress_bytes.as_slice())
            {
                transaction.commit().map_err(|_| unavailable())?;
                return Ok(StoreRetention::Replay);
            }
            return Err(conflict());
        }
        if binding.state != BatchState::ValidationPending
            || binding.active_batch_id.as_ref() != Some(&batch_id)
            || binding.checkpoint_revision.as_ref() != Some(&intent.result_revision)
            || binding.checkpoint_delta_digest.as_ref() != Some(&intent.delta_digest)
        {
            return Err(conflict());
        }
        let changed = transaction
            .execute(
                "UPDATE change_batch_workspace SET state = 'observation_pending', updated_at = ?3
                 WHERE workspace_id = ?1 AND active_batch_id = ?2 AND state = 'validation_pending'",
                params![workspace_id, batch_id.0, now.0],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err(conflict());
        }
        transaction
            .execute(
                "INSERT INTO change_batch_observation
                   (batch_id, observation_id, request_json, request_digest, confirmed_sequence,
                    response_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 0, x'', ?5, ?5)",
                params![
                    batch_id.0,
                    intent.observation_id.0,
                    request_bytes,
                    request_digest.0,
                    now.0
                ],
            )
            .map_err(|_| unavailable())?;
        insert_progress(&transaction, progress, &progress_bytes)?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(StoreRetention::Inserted)
    }

    /// Loads and revalidates one exact durable Observer request.
    ///
    /// # Errors
    ///
    /// Rejects corrupt stored state.
    pub fn observation_request(
        &self,
        batch_id: &ChangeBatchId,
    ) -> Result<Option<ObservationRequest>, ChangeBatchStoreError> {
        let bytes = self
            .connection
            .query_row(
                "SELECT request_json FROM change_batch_observation WHERE batch_id = ?1",
                params![batch_id.0],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(|_| unavailable())?
            .flatten();
        bytes.map(|bytes| decode(&bytes)).transpose()
    }

    /// Retains the exact one-shot Provider open before it may be sent.
    ///
    /// # Errors
    ///
    /// Rejects a foreign intent, an invalid open, or changed replay bytes.
    pub fn retain_observation_model_open(
        &mut self,
        batch_id: &ChangeBatchId,
        open: &ModelOpenMessage,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        let request = self.observation_request(batch_id)?.ok_or_else(conflict)?;
        if open.lease.job_id != request.intent.identity.job_id
            || open.lease.attempt != request.intent.identity.attempt
            || open.lease.lease_id != request.intent.identity.lease_id
            || open.lease.fencing_token != request.intent.identity.fencing_token
            || open.session_identity != request.intent.identity.session_identity
            || open.worker_session_id != request.intent.identity.session_identity.worker_session_id
            || open.sent_at != *now
            || stream_key_from_message(&ExecutionPortMessage::ModelOpenMessage(open.clone()))
                .is_err()
        {
            return Err(invalid("ChangeBatch Observer model open is invalid"));
        }
        let bytes = canonical_bytes(open)?;
        let existing = self
            .connection
            .query_row(
                "SELECT model_exchange_id, model_open_json FROM change_batch_observation
                 WHERE batch_id = ?1",
                params![batch_id.0],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(conflict)?;
        if let (Some(exchange), Some(existing_bytes)) = existing {
            if exchange == open.model_exchange_id.0 && existing_bytes == bytes {
                return Ok(StoreRetention::Replay);
            }
            return Err(conflict());
        }
        let changed = self
            .connection
            .execute(
                "UPDATE change_batch_observation
                 SET model_exchange_id = ?2, model_open_json = ?3, updated_at = ?4
                 WHERE batch_id = ?1 AND model_exchange_id IS NULL AND model_open_json IS NULL",
                params![batch_id.0, open.model_exchange_id.0, bytes, now.0],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err(conflict());
        }
        Ok(StoreRetention::Inserted)
    }

    /// Loads and revalidates durable Provider exchange state by model identity.
    ///
    /// # Errors
    ///
    /// Rejects corrupt canonical bytes or an invalid retained receipt.
    pub fn observation_model_record(
        &self,
        model_exchange_id: &ModelExchangeId,
    ) -> Result<Option<ObservationModelRecord>, ChangeBatchStoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT batch_id, model_open_json, confirmed_sequence, response_json,
                        model_usage_json, terminal_status, receipt_json
                 FROM change_batch_observation WHERE model_exchange_id = ?1",
                params![model_exchange_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<Vec<u8>>>(6)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?;
        let Some((batch, open_bytes, confirmed, response, usage, terminal, receipt_bytes)) = row
        else {
            return Ok(None);
        };
        let batch_id = ChangeBatchId(batch);
        let request = self.observation_request(&batch_id)?.ok_or_else(corrupt)?;
        let model_open = open_bytes
            .map(|bytes| {
                let open: ModelOpenMessage = decode(&bytes)?;
                if open.model_exchange_id != *model_exchange_id
                    || open.lease.job_id != request.intent.identity.job_id
                    || open.lease.attempt != request.intent.identity.attempt
                    || open.lease.lease_id != request.intent.identity.lease_id
                    || open.lease.fencing_token != request.intent.identity.fencing_token
                    || open.session_identity != request.intent.identity.session_identity
                    || stream_key_from_message(&ExecutionPortMessage::ModelOpenMessage(
                        open.clone(),
                    ))
                    .is_err()
                {
                    return Err(corrupt());
                }
                Ok(open)
            })
            .transpose()?;
        let model_usage = usage
            .map(|bytes| {
                let usage: winwincode_execution_port::generated::ExecutionOutcomeUsage =
                    decode(&bytes)?;
                if usage.tokens < 0 || usage.runtime_millis < 0 || usage.cost_microunits < 0 {
                    return Err(corrupt());
                }
                Ok(usage)
            })
            .transpose()?;
        let receipt = receipt_bytes
            .map(|bytes| {
                let receipt: ObservationReceipt = decode(&bytes)?;
                validate_observation_receipt(&receipt, &request.intent).map_err(|_| corrupt())?;
                Ok(receipt)
            })
            .transpose()?;
        Ok(Some(ObservationModelRecord {
            request,
            model_open,
            confirmed_sequence: confirmed,
            response_bytes: response.unwrap_or_default(),
            model_usage,
            terminal_status: terminal,
            receipt,
        }))
    }

    /// Returns the unfinished or pending-terminal Observer open for one Job.
    ///
    /// # Errors
    ///
    /// Rejects corrupt state or an unbounded recovery backlog.
    pub fn pending_observation_model_open(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ObservationModelRecord>, ChangeBatchStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT observation.model_exchange_id
                 FROM change_batch_observation AS observation
                 WHERE observation.model_exchange_id IS NOT NULL
                   AND (observation.receipt_json IS NULL
                        OR EXISTS (SELECT 1 FROM change_batch_workspace
                                   WHERE active_batch_id = observation.batch_id
                                     AND state = 'observation_pending'))
                 ORDER BY created_at, observation_id",
            )
            .map_err(|_| unavailable())?;
        let exchanges = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| unavailable())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| unavailable())?;
        self.single_exchange_for_job(job_id, &exchanges)
    }

    /// Returns the already-cancelled Observer open for one Job.
    ///
    /// # Errors
    ///
    /// Rejects corrupt state or an unbounded recovery backlog.
    pub fn cancelled_observation_model_open(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ObservationModelRecord>, ChangeBatchStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT model_exchange_id FROM change_batch_observation
                 WHERE terminal_status = 'provider_error' AND receipt_json IS NULL
                   AND model_exchange_id IS NOT NULL
                 ORDER BY updated_at DESC, observation_id",
            )
            .map_err(|_| unavailable())?;
        let exchanges = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|_| unavailable())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| unavailable())?;
        self.single_exchange_for_job(job_id, &exchanges)
    }

    fn single_exchange_for_job(
        &self,
        job_id: &ExecutionJobId,
        exchanges: &[String],
    ) -> Result<Option<ObservationModelRecord>, ChangeBatchStoreError> {
        if exchanges.len() > MAX_RETAINED_RECORDS {
            return Err(ChangeBatchStoreError::new(
                "ChangeBatch Observer recovery backlog is too large",
            ));
        }
        let mut matching = Vec::new();
        for exchange in exchanges {
            let record = self
                .observation_model_record(&ModelExchangeId(exchange.clone()))?
                .ok_or_else(corrupt)?;
            if record.request.intent.identity.job_id == *job_id {
                matching.push(record);
            }
        }
        if matching.len() > 1 {
            return Err(corrupt());
        }
        Ok(matching.pop())
    }

    /// Marks an unfinished Observer exchange terminal before Job cancellation.
    ///
    /// # Errors
    ///
    /// Rejects an unknown or already-terminal exchange.
    pub fn cancel_observation_model(
        &mut self,
        batch_id: &ChangeBatchId,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        if now.0.is_empty() {
            return Err(invalid("ChangeBatch Observer cancellation is invalid"));
        }
        let changed = self
            .connection
            .execute(
                "UPDATE change_batch_observation
                 SET terminal_status = 'provider_error', updated_at = ?2
                 WHERE batch_id = ?1 AND terminal_status IS NULL",
                params![batch_id.0, now.0],
            )
            .map_err(|_| unavailable())?;
        if changed == 1 {
            Ok(StoreRetention::Inserted)
        } else if changed == 0 {
            Ok(StoreRetention::Replay)
        } else {
            Err(conflict())
        }
    }

    /// Retains one contiguous, content-bound Observer model frame.
    ///
    /// Only the strict response text, terminal usage, and stable terminal kind
    /// are retained. Provider reasoning, tool data, and raw envelopes never
    /// enter the store.
    ///
    /// # Errors
    ///
    /// Rejects an unknown exchange, changed replay bytes, or a second terminal
    /// frame.
    pub fn retain_observation_model_chunk(
        &mut self,
        frame: &ObservationModelFrame<'_>,
        now: &Instant,
    ) -> Result<ObservationChunkRetention, ChangeBatchStoreError> {
        if !valid_frame(frame) || now.0.is_empty() {
            return Err(invalid("ChangeBatch Observer model chunk is invalid"));
        }
        let (sequence, chunk_digest, response_delta) =
            (frame.sequence, &frame.chunk_digest, frame.response_delta);
        let model_usage = frame.model_usage.as_ref();
        let terminal_status = frame.terminal_status;
        let model_exchange_id = &frame.model_exchange_id;
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let Some((observation_id, confirmed, existing_response, existing_terminal)) = transaction
            .query_row(
                "SELECT observation_id, confirmed_sequence, response_json, terminal_status
                 FROM change_batch_observation WHERE model_exchange_id = ?1",
                params![model_exchange_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?.unwrap_or_default(),
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?
        else {
            return Err(conflict());
        };
        let existing_digest = transaction
            .query_row(
                "SELECT chunk_digest FROM change_batch_observation_chunk
                 WHERE observation_id = ?1 AND sequence = ?2",
                params![observation_id, sequence],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| unavailable())?;
        if let Some(existing_digest) = existing_digest {
            if existing_digest != chunk_digest.0 || sequence > confirmed {
                return Err(conflict());
            }
            transaction.commit().map_err(|_| unavailable())?;
            return Ok(ObservationChunkRetention::Duplicate {
                confirmed_sequence: confirmed,
            });
        }
        if sequence <= confirmed {
            return Err(corrupt());
        }
        if sequence != confirmed.saturating_add(1) {
            transaction.commit().map_err(|_| unavailable())?;
            return Ok(ObservationChunkRetention::Gap {
                confirmed_sequence: confirmed,
            });
        }
        if existing_terminal.is_some() {
            return Err(conflict());
        }
        let mut response = existing_response;
        response.extend_from_slice(response_delta);
        if response.len() > MAX_OBSERVATION_RESPONSE_BYTES {
            return Err(invalid("ChangeBatch Observer response is too large"));
        }
        let usage_bytes = model_usage.map(canonical_bytes).transpose()?;
        transaction
            .execute(
                "INSERT INTO change_batch_observation_chunk
                   (observation_id, sequence, chunk_digest) VALUES (?1, ?2, ?3)",
                params![observation_id, sequence, chunk_digest.0],
            )
            .map_err(|_| unavailable())?;
        let changed = transaction
            .execute(
                "UPDATE change_batch_observation
                 SET confirmed_sequence = ?2, response_json = ?3,
                     model_usage_json = COALESCE(?4, model_usage_json),
                     terminal_status = COALESCE(?5, terminal_status), updated_at = ?6
                 WHERE model_exchange_id = ?1 AND confirmed_sequence = ?7
                   AND terminal_status IS NULL",
                params![
                    model_exchange_id.0,
                    sequence,
                    response,
                    usage_bytes,
                    terminal_status,
                    now.0,
                    confirmed,
                ],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Err(conflict());
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(ObservationChunkRetention::Inserted {
            confirmed_sequence: sequence,
        })
    }

    /// Retains the unique exact Observer receipt after a terminal response.
    ///
    /// # Errors
    ///
    /// Rejects an invalid binding, a non-terminal exchange, or changed replay.
    pub fn retain_observation_receipt(
        &mut self,
        receipt: &ObservationReceipt,
        now: &Instant,
    ) -> Result<StoreRetention, ChangeBatchStoreError> {
        let batch_id = receipt.identity.batch_id.clone();
        let request = self.observation_request(&batch_id)?.ok_or_else(conflict)?;
        validate_observation_receipt(receipt, &request.intent)
            .map_err(|_| invalid("ChangeBatch Observer receipt is invalid"))?;
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let observer_row = transaction
            .query_row(
                "SELECT receipt_json, terminal_status FROM change_batch_observation
                 WHERE batch_id = ?1",
                params![batch_id.0],
                |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| unavailable())?
            .ok_or_else(conflict)?;
        if observer_row.1.is_none() {
            return Err(conflict());
        }
        if let Some(existing) = observer_row.0.as_ref() {
            if decode::<ObservationReceipt>(existing).ok().as_ref() == Some(receipt) {
                transaction.commit().map_err(|_| unavailable())?;
                return Ok(StoreRetention::Replay);
            }
            return Err(conflict());
        }
        let mut record = load_record(&transaction, &batch_id)?;
        let Some(stored) = record.receipt.as_mut() else {
            return Err(conflict());
        };
        if stored.observation.as_ref() == Some(receipt) {
            transaction.commit().map_err(|_| unavailable())?;
            return Ok(StoreRetention::Replay);
        }
        if stored.observation.is_some() || now.0.is_empty() {
            return Err(conflict());
        }
        stored.observation = Some(receipt.clone());
        validate_receipt(stored, &record.event.identity, &record.base_revision)?;
        let upgraded = canonical_bytes(stored)?;
        let observer_bytes = canonical_bytes(receipt)?;
        let observer_changed = transaction
            .execute(
                "UPDATE change_batch_observation SET receipt_json = ?2, updated_at = ?3
                 WHERE batch_id = ?1 AND receipt_json IS NULL AND terminal_status IS NOT NULL",
                params![batch_id.0, observer_bytes, now.0],
            )
            .map_err(|_| unavailable())?;
        let execution_changed = transaction
            .execute(
                "UPDATE change_batch_intent SET receipt_json = ?2, updated_at = ?3
                 WHERE batch_id = ?1",
                params![batch_id.0, upgraded, now.0],
            )
            .map_err(|_| unavailable())?;
        if observer_changed != 1 || execution_changed != 1 {
            return Err(conflict());
        }
        transaction.commit().map_err(|_| unavailable())?;
        Ok(StoreRetention::Inserted)
    }

    /// Accepts one checkpoint only after an exact typed observation fact.
    ///
    /// Stale or foreign revision and digest facts return `Stale` without
    /// changing the accepted revision or releasing the active batch.
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
    ) -> Result<ObservationGateResult, ChangeBatchStoreError> {
        require_valid_progress(progress)?;
        if progress.state != ChangeBatchProgressState::Accepted || now.0.is_empty() {
            return Err(invalid("ChangeBatch acceptance progress is invalid"));
        }
        if !valid_revision(observed_revision) || !valid_digest(observed_delta_digest) {
            return Err(invalid("ChangeBatch acceptance checkpoint is invalid"));
        }
        let progress_bytes = canonical_bytes(progress)?;
        let batch_id = progress.identity.batch_id.clone();
        let transaction = self.connection.transaction().map_err(|_| unavailable())?;
        let record = load_record(&transaction, &batch_id)?;
        if record.event.identity != progress.identity {
            return Ok(ObservationGateResult::Stale);
        }
        let binding = load_binding(&transaction, workspace_id)?;
        let existing_progress = load_progress_bytes(&transaction, &batch_id, progress.sequence)?;
        if binding.state == BatchState::Accepted
            && binding.active_batch_id.is_none()
            && binding.accepted_revision == *observed_revision
            && existing_progress.as_deref() == Some(progress_bytes.as_slice())
            && receipt_delta_matches(record.receipt.as_ref(), observed_delta_digest)
        {
            transaction.commit().map_err(|_| unavailable())?;
            return Ok(ObservationGateResult::Accepted);
        }
        if binding.state != BatchState::ObservationPending
            && binding.state != BatchState::ValidationPending
            || binding.active_batch_id.as_ref() != Some(&batch_id)
            || binding.checkpoint_revision.as_ref() != Some(observed_revision)
            || binding.checkpoint_delta_digest.as_ref() != Some(observed_delta_digest)
            || existing_progress.is_some()
        {
            return Ok(ObservationGateResult::Stale);
        }
        let changed = transaction
            .execute(
                "UPDATE change_batch_workspace
                 SET accepted_revision = checkpoint_revision, active_batch_id = NULL,
                     state = 'accepted', checkpoint_revision = NULL,
                     checkpoint_delta_digest = NULL, updated_at = ?3
                 WHERE workspace_id = ?1 AND active_batch_id = ?2
                   AND state IN ('validation_pending', 'observation_pending')
                   AND checkpoint_revision = ?4 AND checkpoint_delta_digest = ?5",
                params![
                    workspace_id,
                    batch_id.0,
                    now.0,
                    observed_revision.0,
                    observed_delta_digest.0,
                ],
            )
            .map_err(|_| unavailable())?;
        if changed != 1 {
            return Ok(ObservationGateResult::Stale);
        }
        transaction
            .execute(
                "UPDATE change_batch_workspace SET updated_at = ?2 WHERE workspace_id = ?1",
                params![workspace_id, now.0],
            )
            .map_err(|_| unavailable())?;
        insert_progress(&transaction, progress, &progress_bytes)?;
        transaction.commit().map_err(|_| unavailable())?;
        Ok(ObservationGateResult::Accepted)
    }
}

/// Sorts and validates secret-safe applied-file summaries for one receipt.
///
/// # Errors
///
/// Rejects more than 20 touched paths, conflicting source/destination paths,
/// non-portable paths, non-canonical digests or modes, negative byte counts,
/// and operation/optional-field combinations that cannot represent an exact
/// Add, Update, Delete, or Move.
pub fn canonical_applied_file_summaries(
    summaries: &[AppliedFileSummary],
) -> Result<Vec<AppliedFileSummary>, ChangeBatchStoreError> {
    const MAX_DELTA_FILES: usize = 20;
    let mut result = summaries.to_vec();
    let mut touched = HashSet::new();
    for summary in &result {
        validate_applied_summary(summary)?;
        claim_delta_path(&mut touched, &summary.path)?;
        if let Some(move_path) = &summary.move_path {
            claim_delta_path(&mut touched, move_path)?;
        }
        if touched.len() > MAX_DELTA_FILES {
            return Err(invalid("ChangeBatch delta exceeds its file bound"));
        }
    }
    result.sort_by(compare_summaries);
    Ok(result)
}

/// Derives a stable digest from canonical sorted file summaries using a domain
/// separator, explicit optional-value tags, and unsigned 64-bit length framing.
///
/// # Errors
///
/// Returns the same strict validation failures as
/// [`canonical_applied_file_summaries`].
pub fn derive_delta_digest(
    summaries: &[AppliedFileSummary],
) -> Result<Sha256Digest, ChangeBatchStoreError> {
    let summaries = canonical_applied_file_summaries(summaries)?;
    let mut hasher = Sha256::new();
    hasher.update(DELTA_DIGEST_DOMAIN);
    frame_u64(&mut hasher, usize_to_u64(summaries.len()));
    for summary in &summaries {
        frame_bytes(&mut hasher, summary.path.as_bytes());
        hasher.update([operation_tag(&summary.operation)]);
        frame_optional_text(&mut hasher, summary.move_path.as_deref());
        frame_optional_digest(&mut hasher, summary.before_sha256.as_ref());
        frame_optional_digest(&mut hasher, summary.after_sha256.as_ref());
        frame_u64(&mut hasher, i64_to_u64(summary.bytes_before));
        frame_u64(&mut hasher, i64_to_u64(summary.bytes_after));
        frame_optional_text(&mut hasher, summary.mode_before.as_deref());
        frame_optional_text(&mut hasher, summary.mode_after.as_deref());
    }
    Ok(Sha256Digest(format!("sha256:{:x}", hasher.finalize())))
}

fn frame_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(usize_to_u64(bytes.len()).to_be_bytes());
    hasher.update(bytes);
}

fn frame_u64(hasher: &mut Sha256, value: u64) {
    hasher.update(value.to_be_bytes());
}

fn frame_optional_text(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        None => hasher.update([0]),
        Some(value) => {
            hasher.update([1]);
            frame_bytes(hasher, value.as_bytes());
        }
    }
}

fn frame_optional_digest(hasher: &mut Sha256, value: Option<&Sha256Digest>) {
    frame_optional_text(hasher, value.map(|digest| digest.0.as_str()));
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn claim_delta_path(
    touched: &mut HashSet<String>,
    path: &str,
) -> Result<(), ChangeBatchStoreError> {
    if !touched.insert(path.to_owned()) {
        return Err(invalid("ChangeBatch delta claims one path twice"));
    }
    Ok(())
}

fn validate_applied_summary(summary: &AppliedFileSummary) -> Result<(), ChangeBatchStoreError> {
    portable_path(Path::new(&summary.path))?;
    if let Some(move_path) = &summary.move_path {
        portable_path(Path::new(move_path))?;
    }
    let canonical_digest = |digest: &Sha256Digest| {
        digest
            .0
            .strip_prefix("sha256:")
            .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
    };
    let canonical_mode = |mode: &str| {
        matches!(mode.len(), 3 | 4) && mode.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
    };
    if summary.bytes_before < 0
        || summary.bytes_after < 0
        || !summary.before_sha256.as_ref().is_none_or(canonical_digest)
        || !summary.after_sha256.as_ref().is_none_or(canonical_digest)
        || !summary.mode_before.as_deref().is_none_or(canonical_mode)
        || !summary.mode_after.as_deref().is_none_or(canonical_mode)
    {
        return Err(invalid("ChangeBatch applied file summary is invalid"));
    }
    let valid_shape = match summary.operation {
        AppliedFileOperation::Create => {
            summary.move_path.is_none()
                && summary.before_sha256.is_none()
                && summary.after_sha256.is_some()
                && summary.bytes_before == 0
                && summary.mode_before.is_none()
                && summary.mode_after.is_some()
        }
        AppliedFileOperation::Update => {
            summary.move_path.is_none()
                && summary.before_sha256.is_some()
                && summary.after_sha256.is_some()
                && summary.mode_before.is_some()
                && summary.mode_after.is_some()
        }
        AppliedFileOperation::Delete => {
            summary.move_path.is_none()
                && summary.before_sha256.is_some()
                && summary.after_sha256.is_none()
                && summary.bytes_after == 0
                && summary.mode_before.is_some()
                && summary.mode_after.is_none()
        }
        AppliedFileOperation::MoveValue => {
            summary.move_path.is_some()
                && summary.before_sha256.is_some()
                && summary.after_sha256.is_some()
                && summary.mode_before.is_some()
                && summary.mode_after.is_some()
        }
    };
    if !valid_shape {
        return Err(invalid("ChangeBatch applied file summary is invalid"));
    }
    Ok(())
}

fn portable_path(path: &Path) -> Result<(), ChangeBatchStoreError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("ChangeBatch applied file path is not portable UTF-8"))?;
    if text.is_empty()
        || text.len() > MAX_DELTA_PATH_BYTES
        || text.contains(['\\', '\0', '<', '>', ':', '"', '|', '?', '*'])
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || path.components().any(|component| {
            let std::path::Component::Normal(component) = component else {
                return true;
            };
            let component = component.to_string_lossy();
            component.ends_with([' ', '.']) || windows_reserved_component(&component)
        })
    {
        return Err(invalid("ChangeBatch applied file path is not portable"));
    }
    let canonical = path
        .components()
        .map(std::path::Component::as_os_str)
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if canonical != text {
        return Err(invalid("ChangeBatch applied file path is not portable"));
    }
    Ok(())
}

fn windows_reserved_component(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                suffix.len() <= 2 && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
}

fn compare_summaries(left: &AppliedFileSummary, right: &AppliedFileSummary) -> std::cmp::Ordering {
    (
        left.path.as_str(),
        operation_tag(&left.operation),
        left.move_path.as_deref().unwrap_or(""),
    )
        .cmp(&(
            right.path.as_str(),
            operation_tag(&right.operation),
            right.move_path.as_deref().unwrap_or(""),
        ))
}

const fn operation_tag(operation: &AppliedFileOperation) -> u8 {
    match operation {
        AppliedFileOperation::Create => 0,
        AppliedFileOperation::Update => 1,
        AppliedFileOperation::Delete => 2,
        AppliedFileOperation::MoveValue => 3,
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS change_batch_workspace (
    workspace_id TEXT PRIMARY KEY,
    accepted_revision TEXT NOT NULL,
    active_batch_id TEXT,
    state TEXT NOT NULL,
    checkpoint_revision TEXT,
    checkpoint_delta_digest TEXT,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS change_batch_intent (
    batch_id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    event_json BLOB NOT NULL,
    base_revision TEXT NOT NULL,
    plan_digest TEXT NOT NULL,
    receipt_json BLOB,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS change_batch_intent_job
    ON change_batch_intent (job_id, created_at);
CREATE TABLE IF NOT EXISTS change_batch_progress (
    batch_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    event_json BLOB NOT NULL,
    PRIMARY KEY (batch_id, sequence)
);
CREATE TABLE IF NOT EXISTS change_batch_observation (
    batch_id TEXT PRIMARY KEY,
    observation_id TEXT NOT NULL,
    request_json BLOB NOT NULL,
    request_digest TEXT NOT NULL,
    model_exchange_id TEXT UNIQUE,
    model_open_json BLOB,
    confirmed_sequence INTEGER NOT NULL DEFAULT 0,
    response_json BLOB NOT NULL DEFAULT x'',
    model_usage_json BLOB,
    terminal_status TEXT,
    receipt_json BLOB,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS change_batch_observation_pending
    ON change_batch_observation (created_at, observation_id);
CREATE TABLE IF NOT EXISTS change_batch_observation_chunk (
    observation_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    chunk_digest TEXT NOT NULL,
    PRIMARY KEY (observation_id, sequence)
);
CREATE TABLE IF NOT EXISTS change_batch_diagnostic (
    batch_id TEXT PRIMARY KEY,
    base_revision TEXT NOT NULL,
    result_revision TEXT NOT NULL,
    baseline_json BLOB,
    evaluation_json BLOB,
    updated_at TEXT NOT NULL
);
";

fn ensure_private_directory(path: &Path) -> Result<(), ChangeBatchStoreError> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            let permissions = fs::metadata(path).map_err(|_| unavailable())?.permissions();
            if permissions.mode() & 0o077 != 0 {
                return Err(ChangeBatchStoreError::new(
                    "ChangeBatch store directory is not private",
                ));
            }
            Ok(())
        }
        Ok(_) => Err(ChangeBatchStoreError::new(
            "ChangeBatch store path is not a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| unavailable()),
        Err(_) => Err(unavailable()),
    }
}

fn ensure_private_file(path: &Path) -> Result<(), ChangeBatchStoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(ChangeBatchStoreError::new(
            "ChangeBatch store database is not a regular file",
        )),
        Err(_) => Ok(()),
    }
}

fn load_binding(
    connection: &Connection,
    workspace_id: &str,
) -> Result<WorkspaceBatchBinding, ChangeBatchStoreError> {
    let row = connection
        .query_row(
            "SELECT accepted_revision, active_batch_id, state, checkpoint_revision,
                    checkpoint_delta_digest
             FROM change_batch_workspace WHERE workspace_id = ?1",
            params![workspace_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| unavailable())?;
    let Some((
        accepted_revision,
        active_batch_id,
        state,
        checkpoint_revision,
        checkpoint_delta_digest,
    )) = row
    else {
        return Err(conflict());
    };
    let Some(state) = BatchState::parse(&state) else {
        return Err(corrupt());
    };
    Ok(WorkspaceBatchBinding {
        workspace_id: workspace_id.to_owned(),
        accepted_revision: WorkspaceRevision(accepted_revision),
        active_batch_id: active_batch_id.map(ChangeBatchId),
        state,
        checkpoint_revision: checkpoint_revision.map(WorkspaceRevision),
        checkpoint_delta_digest: checkpoint_delta_digest.map(Sha256Digest),
    })
}

struct StoredRecord {
    event: ChangeBatchProposalEvent,
    base_revision: WorkspaceRevision,
    receipt: Option<ChangeBatchReceipt>,
    workspace_id: String,
}

/// Mints the workspace accepted-revision anchor on its first claim, and
/// rejects any claim whose base revision contradicts the anchored revision.
fn mint_workspace_anchor(
    connection: &Connection,
    workspace_id: &str,
    expected_base_revision: &WorkspaceRevision,
    now: &Instant,
) -> Result<(), ChangeBatchStoreError> {
    let anchored = connection
        .query_row(
            "SELECT accepted_revision FROM change_batch_workspace WHERE workspace_id = ?1",
            params![workspace_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|_| unavailable())?;
    match anchored {
        None => {
            connection
                .execute(
                    "INSERT INTO change_batch_workspace
                       (workspace_id, accepted_revision, active_batch_id, state,
                        checkpoint_revision, checkpoint_delta_digest, updated_at)
                     VALUES (?1, ?2, NULL, 'idle', NULL, NULL, ?3)",
                    params![workspace_id, expected_base_revision.0, now.0],
                )
                .map_err(|_| unavailable())?;
        }
        Some(stored) if WorkspaceRevision(stored.clone()) == *expected_base_revision => {}
        Some(_) => return Err(conflict()),
    }
    Ok(())
}

fn load_record(
    connection: &Connection,
    batch_id: &ChangeBatchId,
) -> Result<StoredRecord, ChangeBatchStoreError> {
    let row = connection
        .query_row(
            "SELECT event_json, base_revision, receipt_json, workspace_id
             FROM change_batch_intent WHERE batch_id = ?1",
            params![batch_id.0],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|_| unavailable())?
        .ok_or_else(conflict)?;
    Ok(StoredRecord {
        event: decode(&row.0)?,
        base_revision: WorkspaceRevision(row.1),
        receipt: row.2.map(|bytes| decode(&bytes)).transpose()?,
        workspace_id: row.3,
    })
}

/// Decodes every stored progress fact of one batch in sequence order.
fn stored_progress(
    connection: &Connection,
    batch_id: &ChangeBatchId,
) -> Result<Vec<ChangeBatchProgressEvent>, ChangeBatchStoreError> {
    let mut statement = connection
        .prepare(
            "SELECT event_json FROM change_batch_progress
             WHERE batch_id = ?1 ORDER BY sequence",
        )
        .map_err(|_| unavailable())?;
    let rows = statement
        .query_map(params![batch_id.0], |row| row.get::<_, Vec<u8>>(0))
        .map_err(|_| unavailable())?;
    rows.map(|row| {
        let bytes = row.map_err(|_| unavailable())?;
        decode::<ChangeBatchProgressEvent>(&bytes)
    })
    .collect()
}

/// Replays one batch's stored ledger and validates the new fact against it
/// before anything is written, so an out-of-sequence, identity-changing,
/// illegal, or post-terminal fact can never become durable.
fn checked_progress_ledger(
    connection: &Connection,
    event: &ChangeBatchProgressEvent,
) -> Result<(), ChangeBatchStoreError> {
    let mut ledger = ChangeBatchProgressLedger::new();
    for stored in stored_progress(connection, &event.identity.batch_id)? {
        ledger.record(&stored).map_err(|_| corrupt())?;
    }
    ledger.record(event).map_err(|_| conflict())
}

fn insert_progress(
    connection: &Connection,
    event: &ChangeBatchProgressEvent,
    bytes: &[u8],
) -> Result<StoreRetention, ChangeBatchStoreError> {
    checked_progress_ledger(connection, event)?;
    let changed = connection
        .execute(
            "INSERT INTO change_batch_progress (batch_id, sequence, event_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(batch_id, sequence) DO NOTHING",
            params![event.identity.batch_id.0, event.sequence, bytes],
        )
        .map_err(|_| unavailable())?;
    if changed == 1 {
        Ok(StoreRetention::Inserted)
    } else {
        Err(conflict())
    }
}

fn load_progress_bytes(
    connection: &Connection,
    batch_id: &ChangeBatchId,
    sequence: i64,
) -> Result<Option<Vec<u8>>, ChangeBatchStoreError> {
    connection
        .query_row(
            "SELECT event_json FROM change_batch_progress
             WHERE batch_id = ?1 AND sequence = ?2",
            params![batch_id.0, sequence],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()
        .map_err(|_| unavailable())
}

/// Bounded shape checks for one submitted Provider frame.
fn valid_frame(frame: &ObservationModelFrame<'_>) -> bool {
    valid_progress_sequence(frame.sequence)
        && valid_digest(&frame.chunk_digest)
        && frame.model_exchange_id.0.len() <= 256
        && frame.model_usage.as_ref().is_none_or(|usage| {
            usage.tokens >= 0 && usage.runtime_millis >= 0 && usage.cost_microunits >= 0
        })
        && frame
            .terminal_status
            .is_none_or(|value| matches!(value, "completed" | "provider_error"))
}

fn require_valid_progress(event: &ChangeBatchProgressEvent) -> Result<(), ChangeBatchStoreError> {
    if !valid_progress_event(event) {
        return Err(invalid("ChangeBatch progress is invalid"));
    }
    Ok(())
}

/// Checks a receipt against the canonical receipt rules before it may be
/// retained: the identity and base revision must bind the retained proposal,
/// and each status must claim exactly the proven tree it is allowed to claim.
fn validate_receipt(
    receipt: &ChangeBatchReceipt,
    identity: &ChangeBatchIdentity,
    base_revision: &WorkspaceRevision,
) -> Result<(), ChangeBatchStoreError> {
    if receipt.identity != *identity || receipt.base_revision != *base_revision {
        return Err(conflict());
    }
    let files_empty = receipt.files.is_empty();
    match receipt.status {
        ChangeBatchReceiptStatus::Applied | ChangeBatchReceiptStatus::PartiallyApplied => {
            if files_empty
                || !receipt.result_revision.as_ref().is_some_and(valid_revision)
                || !receipt.delta_digest.as_ref().is_some_and(valid_digest)
                || !receipt.delta_exact
            {
                return Err(invalid("ChangeBatch receipt does not bind an exact delta"));
            }
        }
        ChangeBatchReceiptStatus::Rejected => {
            if !files_empty || receipt.result_revision.is_some() || receipt.delta_digest.is_some() {
                return Err(invalid("ChangeBatch rejected receipt binds no proven tree"));
            }
        }
        ChangeBatchReceiptStatus::StateUncertain => {
            if receipt.result_revision.is_some() || receipt.delta_digest.is_some() {
                return Err(invalid(
                    "ChangeBatch uncertain receipt forbids result claims",
                ));
            }
        }
    }
    Ok(())
}

fn receipt_delta_matches(
    receipt: Option<&ChangeBatchReceipt>,
    delta_digest: &Sha256Digest,
) -> bool {
    receipt
        .and_then(|receipt| receipt.delta_digest.as_ref())
        .is_some_and(|stored| stored == delta_digest)
}

fn digest_bytes(bytes: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Sha256Digest(format!("sha256:{:x}", hasher.finalize()))
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ChangeBatchStoreError> {
    serde_json::from_slice(bytes).map_err(|_| corrupt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_domain::{
        CodexThreadId, ExecutionMessageId, FencingToken, LeaseId, ProductSessionId, RepositoryId,
        RequestId, SchemaVersion, SessionIdentity, WorkerId, WorkerInstanceId, WorkerSessionId,
    };
    use winwincode_execution_port::change_batch_identity::derive_change_batch_id;
    use winwincode_execution_port::generated::{
        AppliedFileOperation, AppliedFileSummary, ChangeBatchProposal,
        ChangeBatchProposalDisposition, EncodedPayload, ExecutionLeaseStamp, ExecutionOutcomeUsage,
        ModelGatewayRoute, ModelOpenMessageKind, ObservationAcceptanceCriterion,
        ObservationDataEgressPolicy, ObservationDecision, ObservationDeltaSummary,
        ObservationIntent, ObservationPromptInjectionScan, ObservationPromptInjectionStatus,
        ObservationReasonCode, ObservationResponse, ObservationSecretScan,
        ObservationSecretScanStatus, ObservationSnippet, ObservationSource,
        ObservationUntrustedInput, ObservationUntrustedInputTrustLevel, RepairClass,
        ValidationProfileName,
    };
    use winwincode_execution_port::observation_contract::{
        derive_observation_content_digest, derive_observation_id, derive_observation_input_digest,
        derive_observation_output_digest, derive_observation_profile_digest,
    };

    const WORKSPACE: &str = "ws-1";
    const BASE: &str = "git-tree:0000000000000000000000000000000000000000";
    const RESULT: &str = "git-tree:ffffffffffffffffffffffffffffffffffffffff";
    const EXCHANGE: &str = "mx-1";

    fn digest(fill: char) -> Sha256Digest {
        Sha256Digest(format!("sha256:{}", fill.to_string().repeat(64)))
    }

    fn revision(encoded: &str) -> WorkspaceRevision {
        WorkspaceRevision(encoded.to_owned())
    }

    fn instant() -> Instant {
        Instant("2026-09-04T00:00:00.000Z".to_owned())
    }

    fn identity() -> ChangeBatchIdentity {
        let patch_digest = digest('1');
        ChangeBatchIdentity {
            attempt: 1,
            batch_id: derive_change_batch_id("run-key-1", "turn-1", None, &patch_digest)
                .expect("derive batch identity"),
            call_id: None,
            fencing_token: FencingToken("1".to_owned()),
            job_id: ExecutionJobId("job_00000000000000000000000000".to_owned()),
            lease_id: LeaseId("lse_00000000000000000000000000".to_owned()),
            patch_digest,
            repository_id: RepositoryId("rep_00000000000000000000000000".to_owned()),
            run_key: "run-key-1".to_owned(),
            session_identity: SessionIdentity {
                codex_thread_id: CodexThreadId("cdx_00000000000000000000000000".to_owned()),
                product_session_id: ProductSessionId("psn_00000000000000000000000000".to_owned()),
                stage_run_id: None,
                worker_session_id: WorkerSessionId("wsn_00000000000000000000000000".to_owned()),
            },
            turn_id: "turn-1".to_owned(),
            workspace_revision: revision(BASE),
        }
    }

    /// A second, distinct proposal for the same workspace.
    fn other_identity(fill: char) -> ChangeBatchIdentity {
        let mut identity = identity();
        identity.patch_digest = digest(fill);
        identity.batch_id = derive_change_batch_id(
            &identity.run_key,
            &identity.turn_id,
            None,
            &identity.patch_digest,
        )
        .expect("derive other batch identity");
        identity
    }

    fn proposal_event(identity: &ChangeBatchIdentity) -> ChangeBatchProposalEvent {
        ChangeBatchProposalEvent {
            identity: identity.clone(),
            occurred_at: instant(),
            proposal: ChangeBatchProposal {
                acceptance_criteria_ids: vec!["criterion-1".to_owned()],
                disposition: ChangeBatchProposalDisposition::Final,
                patch: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,2 @@\n fn answer() -> i32 { 42 }\n".to_owned(),
                schema_version: 1,
                validation_profile: ValidationProfileName::Fast,
            },
        }
    }

    fn progress_event(
        identity: &ChangeBatchIdentity,
        sequence: i64,
        state: ChangeBatchProgressState,
        summary: &str,
    ) -> ChangeBatchProgressEvent {
        ChangeBatchProgressEvent {
            artifact_refs: Vec::new(),
            identity: identity.clone(),
            occurred_at: instant(),
            sequence,
            state,
            summary: summary.to_owned(),
        }
    }

    fn applied_receipt(identity: &ChangeBatchIdentity) -> ChangeBatchReceipt {
        ChangeBatchReceipt {
            artifact_ref: None,
            base_revision: revision(BASE),
            delta_digest: Some(digest('3')),
            delta_exact: true,
            files: vec![AppliedFileSummary {
                after_sha256: Some(digest('a')),
                before_sha256: Some(digest('b')),
                bytes_after: 3,
                bytes_before: 2,
                mode_after: None,
                mode_before: None,
                move_path: None,
                operation: AppliedFileOperation::Update,
                path: "src/lib.rs".to_owned(),
            }],
            identity: identity.clone(),
            normalizer: None,
            observation: None,
            result_revision: Some(revision(RESULT)),
            status: ChangeBatchReceiptStatus::Applied,
            validation: None,
        }
    }

    fn observation_intent(identity: &ChangeBatchIdentity) -> ObservationIntent {
        let result_revision = revision(RESULT);
        let profile_digest = derive_observation_profile_digest(
            &ValidationProfileName::Fast,
            &digest('2'),
            &["cargo-check".to_owned()],
        )
        .expect("profile digest");
        let observation_id =
            derive_observation_id(&identity.batch_id, &result_revision, &profile_digest)
                .expect("observation ID");
        let snippet_content = "pub fn answer() -> i32 { 42 }".to_owned();
        let snippet_digest = Sha256Digest(format!(
            "sha256:{:x}",
            Sha256::digest(snippet_content.as_bytes())
        ));
        let mut untrusted_input = ObservationUntrustedInput {
            acceptance_criteria: vec![ObservationAcceptanceCriterion {
                id: "criterion-1".to_owned(),
                summary: "The requested behavior is present.".to_owned(),
            }],
            batch_summary: "One bounded source update.".to_owned(),
            content_digest: digest('0'),
            delta: ObservationDeltaSummary {
                delta_digest: digest('3'),
                delta_exact: true,
                file_count: 1,
                hunk_count: 1,
                summary: "One exact source-file delta.".to_owned(),
            },
            failed_tests: Vec::new(),
            goal_summary: "Implement the requested bounded behavior.".to_owned(),
            new_diagnostics: Vec::new(),
            snippets: vec![ObservationSnippet {
                content: snippet_content,
                content_digest: snippet_digest,
                end_line: 1,
                path: "src/lib.rs".to_owned(),
                start_line: 1,
            }],
            trust_level: ObservationUntrustedInputTrustLevel::Untrusted,
        };
        untrusted_input.content_digest =
            derive_observation_content_digest(&untrusted_input).expect("content digest");
        let content_digest = untrusted_input.content_digest.clone();
        let mut intent = ObservationIntent {
            all_checks_executed: true,
            data_egress: ObservationDataEgressPolicy {
                external_artifact_reads_allowed: false,
                network_allowed: false,
                provider_file_uploads_allowed: false,
            },
            delta_digest: digest('3'),
            delta_exact: true,
            hard_check_failed: false,
            identity: identity.clone(),
            input_digest: digest('0'),
            observation_id,
            profile_digest,
            prompt_injection_scan: ObservationPromptInjectionScan {
                finding_count: 0,
                input_digest: content_digest.clone(),
                rules_digest: digest('4'),
                scanner_version: "prompt-rules-v1".to_owned(),
                status: ObservationPromptInjectionStatus::Clean,
            },
            result_revision,
            secret_scan: ObservationSecretScan {
                finding_count: 0,
                input_digest: content_digest.clone(),
                output_digest: content_digest,
                scanner_version: "secret-rules-v1".to_owned(),
                status: ObservationSecretScanStatus::Clean,
            },
            untrusted_input,
            validation_profile: ValidationProfileName::Fast,
        };
        intent.input_digest = derive_observation_input_digest(&intent).expect("input digest");
        intent
    }

    fn observation_request(identity: &ChangeBatchIdentity) -> ObservationRequest {
        ObservationRequest {
            intent: observation_intent(identity),
            one_shot: true,
            schema_version: 1,
        }
    }

    fn accept_receipt(intent: &ObservationIntent) -> ObservationReceipt {
        let response = ObservationResponse {
            confidence_bps: 9_500,
            decision: ObservationDecision::Accept,
            observation_id: intent.observation_id.clone(),
            reason_code: ObservationReasonCode::CriteriaSatisfied,
            repair_class: None::<RepairClass>,
            root_causes: Vec::new(),
            schema_version: 1,
            summary: "The bounded evidence satisfies the acceptance criterion.".to_owned(),
        };
        ObservationReceipt {
            identity: intent.identity.clone(),
            input_digest: intent.input_digest.clone(),
            model_usage: Some(ExecutionOutcomeUsage {
                cost_microunits: 1,
                runtime_millis: 5,
                tokens: 7,
            }),
            output_digest: derive_observation_output_digest(&response).expect("output digest"),
            profile_digest: intent.profile_digest.clone(),
            response,
            result_revision: intent.result_revision.clone(),
            source: ObservationSource::Model,
        }
    }

    fn model_open(identity: &ChangeBatchIdentity) -> ModelOpenMessage {
        ModelOpenMessage {
            kind: ModelOpenMessageKind::ModelOpen,
            lease: ExecutionLeaseStamp {
                attempt: 1,
                expires_at: instant(),
                fencing_token: FencingToken("1".to_owned()),
                issued_at: instant(),
                job_id: identity.job_id.clone(),
                lease_id: identity.lease_id.clone(),
                worker_id: WorkerId("wkr_00000000000000000000000000".to_owned()),
                worker_instance_id: WorkerInstanceId("wsi_00000000000000000000000000".to_owned()),
            },
            message_id: ExecutionMessageId("msg-1".to_owned()),
            model_exchange_id: ModelExchangeId(EXCHANGE.to_owned()),
            request: EncodedPayload {
                content_type: "application/json".to_owned(),
                data_base64: "e30=".to_owned(),
                payload_digest: digest('5'),
            },
            request_id: RequestId("req-1".to_owned()),
            route: ModelGatewayRoute {
                capability: "observation".to_owned(),
                route: "gateway".to_owned(),
            },
            schema_version: SchemaVersion::WinwincodeV1,
            sent_at: instant(),
            session_identity: identity.session_identity.clone(),
            worker_session_id: identity.session_identity.worker_session_id.clone(),
        }
    }

    fn open_store() -> (tempfile::TempDir, ChangeBatchStore) {
        let directory = tempfile::tempdir().expect("temporary store root");
        let store = ChangeBatchStore::open(directory.path()).expect("open store");
        (directory, store)
    }

    /// Retains the workspace anchor and one claimed proposal, ready for progress.
    fn claim(store: &mut ChangeBatchStore) -> ChangeBatchIdentity {
        let identity = identity();
        store
            .retain_workspace_binding(WORKSPACE, &revision(BASE), &instant())
            .expect("retain workspace anchor");
        store
            .retain_claimed_intent(
                WORKSPACE,
                &proposal_event(&identity),
                &revision(BASE),
                &digest('6'),
                &instant(),
            )
            .expect("claim intent");
        identity
    }

    /// Drives the canonical ledger prefix up to `apply_started`.
    fn advance_to_apply_started(store: &mut ChangeBatchStore, identity: &ChangeBatchIdentity) {
        for (sequence, state, summary) in [
            (1, ChangeBatchProgressState::Proposed, "proposed"),
            (2, ChangeBatchProgressState::Authorized, "authorized"),
            (3, ChangeBatchProgressState::ApplyStarted, "apply started"),
        ] {
            store
                .retain_workspace_progress(
                    WORKSPACE,
                    &progress_event(identity, sequence, state, summary),
                    BatchState::Applying,
                    BatchState::Applying,
                )
                .expect("progress");
        }
    }

    /// Drives the canonical ledger prefix through the applied checkpoint, which
    /// leaves the workspace `validation_pending` at progress sequence 4.
    fn advance_to_validation_pending(store: &mut ChangeBatchStore, identity: &ChangeBatchIdentity) {
        advance_to_apply_started(store, identity);
        store
            .retain_applied_checkpoint(
                WORKSPACE,
                &progress_event(identity, 4, ChangeBatchProgressState::Applied, "applied"),
                &applied_receipt(identity),
                &instant(),
            )
            .expect("applied checkpoint");
    }

    /// Records the validation phase of the ledger without changing the
    /// durable checkpoint, leaving the batch ready for its one observation.
    fn advance_to_observation_ready(store: &mut ChangeBatchStore, identity: &ChangeBatchIdentity) {
        store
            .retain_workspace_progress(
                WORKSPACE,
                &progress_event(
                    identity,
                    5,
                    ChangeBatchProgressState::ValidationStarted,
                    "validation started",
                ),
                BatchState::ValidationPending,
                BatchState::ValidationPending,
            )
            .expect("validation started");
        store
            .retain_validated_checkpoint(
                WORKSPACE,
                &progress_event(
                    identity,
                    6,
                    ChangeBatchProgressState::ValidationCompleted,
                    "validation completed",
                ),
                &applied_receipt(identity),
                &instant(),
            )
            .expect("validated checkpoint");
    }

    /// Retains the one-shot Observer request for the accepted checkpoint.
    fn request_observation(store: &mut ChangeBatchStore, identity: &ChangeBatchIdentity) {
        store
            .retain_observation_request(
                WORKSPACE,
                &progress_event(
                    identity,
                    7,
                    ChangeBatchProgressState::ObservationRequested,
                    "observation requested",
                ),
                &observation_request(identity),
                &instant(),
            )
            .expect("retain observation request");
    }

    /// Drives the canonical ledger to the observation phase and retains the
    /// model-open frame, returning the single model exchange in use.
    fn open_observation_exchange(
        store: &mut ChangeBatchStore,
        identity: &ChangeBatchIdentity,
    ) -> ModelExchangeId {
        advance_to_validation_pending(store, identity);
        advance_to_observation_ready(store, identity);
        request_observation(store, identity);
        store
            .retain_observation_model_open(&identity.batch_id, &model_open(identity), &instant())
            .expect("retain model open");
        ModelExchangeId(EXCHANGE.to_owned())
    }

    /// Builds one bounded Provider frame carrying the sequenced `response_delta`.
    fn observation_frame<'frame>(
        exchange: &ModelExchangeId,
        sequence: i64,
        digest_fill: char,
        response_delta: &'frame [u8],
        terminal_status: Option<&'frame str>,
    ) -> ObservationModelFrame<'frame> {
        ObservationModelFrame {
            chunk_digest: digest(digest_fill),
            model_exchange_id: exchange.clone(),
            model_usage: None,
            response_delta,
            sequence,
            terminal_status,
        }
    }

    /// Retains one streamed Provider frame, which never carries terminal state.
    fn retain_observation_chunk(
        store: &mut ChangeBatchStore,
        exchange: &ModelExchangeId,
        sequence: i64,
        digest_fill: char,
        response_delta: &'static [u8],
    ) -> Result<ObservationChunkRetention, ChangeBatchStoreError> {
        store.retain_observation_model_chunk(
            &observation_frame(exchange, sequence, digest_fill, response_delta, None),
            &instant(),
        )
    }

    #[test]
    fn claim_rejects_a_batch_id_that_is_not_canonically_derived() {
        let (_directory, mut store) = open_store();
        store
            .retain_workspace_binding(WORKSPACE, &revision(BASE), &instant())
            .expect("retain workspace anchor");
        let mut event = proposal_event(&identity());
        event.identity.run_key = "a-different-run-key".to_owned();
        let error = store
            .retain_claimed_intent(WORKSPACE, &event, &revision(BASE), &digest('6'), &instant())
            .expect_err("a re-keyed proposal must not be retained");
        assert_eq!(
            error.message(),
            "ChangeBatch identity is not canonically derived"
        );
        assert!(
            store
                .batch_record(&event.identity.batch_id)
                .expect("read")
                .is_none()
        );
    }

    #[test]
    fn progress_starts_at_proposed_and_rejects_skipped_sequences() {
        let (_directory, mut store) = open_store();
        let identity = claim(&mut store);

        let gap = progress_event(&identity, 2, ChangeBatchProgressState::Proposed, "proposed");
        assert_eq!(
            store.append_progress(&gap).expect_err("gap").message(),
            conflict().message()
        );

        let first = progress_event(&identity, 1, ChangeBatchProgressState::Proposed, "proposed");
        assert_eq!(
            store.append_progress(&first).expect("first"),
            StoreRetention::Inserted
        );
        assert_eq!(
            store.append_progress(&first).expect_err("repeat").message(),
            conflict().message()
        );
    }

    #[test]
    fn progress_rejects_illegal_transitions_and_terminal_successors() {
        let (_directory, mut store) = open_store();
        let identity = claim(&mut store);
        store
            .append_progress(&progress_event(
                &identity,
                1,
                ChangeBatchProgressState::Proposed,
                "proposed",
            ))
            .expect("first");

        let illegal = progress_event(
            &identity,
            2,
            ChangeBatchProgressState::ObservationRequested,
            "skipped the apply phase",
        );
        assert_eq!(
            store
                .append_progress(&illegal)
                .expect_err("illegal")
                .message(),
            conflict().message()
        );

        store
            .append_progress(&progress_event(
                &identity,
                2,
                ChangeBatchProgressState::RepairRequired,
                "repair required",
            ))
            .expect("terminal");
        let after = progress_event(
            &identity,
            3,
            ChangeBatchProgressState::Authorized,
            "after terminal",
        );
        assert_eq!(
            store
                .append_progress(&after)
                .expect_err("post-terminal")
                .message(),
            conflict().message()
        );
    }

    #[test]
    fn progress_rejects_a_foreign_identity_and_changed_replay_bytes() {
        let (_directory, mut store) = open_store();
        let identity = claim(&mut store);
        store
            .append_progress(&progress_event(
                &identity,
                1,
                ChangeBatchProgressState::Proposed,
                "proposed",
            ))
            .expect("first");

        let foreign = progress_event(
            &other_identity('9'),
            2,
            ChangeBatchProgressState::Authorized,
            "authorized",
        );
        assert_eq!(
            store
                .append_progress(&foreign)
                .expect_err("identity")
                .message(),
            conflict().message()
        );

        let rewritten = progress_event(
            &identity,
            1,
            ChangeBatchProgressState::Proposed,
            "rewritten summary",
        );
        assert_eq!(
            store
                .append_progress(&rewritten)
                .expect_err("changed bytes")
                .message(),
            conflict().message()
        );
    }

    #[test]
    fn checkpoint_receipt_must_bind_identity_base_and_an_exact_delta() {
        let (_directory, mut store) = open_store();
        let identity = claim(&mut store);
        advance_to_apply_started(&mut store, &identity);
        let foreign = other_identity('7');
        let mut misbound = applied_receipt(&foreign);
        misbound.base_revision = revision(BASE);
        assert_eq!(
            store
                .retain_applied_checkpoint(
                    WORKSPACE,
                    &progress_event(&identity, 4, ChangeBatchProgressState::Applied, "applied"),
                    &misbound,
                    &instant(),
                )
                .expect_err("receipt for another batch")
                .message(),
            conflict().message()
        );

        let mut unproven = applied_receipt(&identity);
        unproven.result_revision = None;
        assert_eq!(
            store
                .retain_applied_checkpoint(
                    WORKSPACE,
                    &progress_event(&identity, 4, ChangeBatchProgressState::Applied, "applied"),
                    &unproven,
                    &instant(),
                )
                .expect_err("receipt without a proven tree")
                .message(),
            corrupt().message()
        );

        store
            .retain_applied_checkpoint(
                WORKSPACE,
                &progress_event(&identity, 4, ChangeBatchProgressState::Applied, "applied"),
                &applied_receipt(&identity),
                &instant(),
            )
            .expect("applied checkpoint");
        let binding = store
            .workspace_binding(WORKSPACE)
            .expect("binding")
            .expect("present");
        assert_eq!(binding.state, BatchState::ValidationPending);
        assert_eq!(
            binding.checkpoint_revision.as_ref(),
            Some(&revision(RESULT))
        );
        assert_eq!(binding.checkpoint_delta_digest.as_ref(), Some(&digest('3')));
    }

    #[test]
    fn observer_chunks_are_only_retained_contiguously() {
        let (_directory, mut store) = open_store();
        let identity = claim(&mut store);
        let exchange = open_observation_exchange(&mut store, &identity);

        let first =
            retain_observation_chunk(&mut store, &exchange, 1, 'a', b"first").expect("first chunk");
        assert_eq!(
            first,
            ObservationChunkRetention::Inserted {
                confirmed_sequence: 1
            }
        );

        let replay = retain_observation_chunk(&mut store, &exchange, 1, 'a', b"first")
            .expect("exact replay");
        assert_eq!(
            replay,
            ObservationChunkRetention::Duplicate {
                confirmed_sequence: 1
            }
        );

        let gap = retain_observation_chunk(&mut store, &exchange, 3, 'c', b"third")
            .expect("a gap is reported, never an error");
        assert_eq!(
            gap,
            ObservationChunkRetention::Gap {
                confirmed_sequence: 1
            }
        );

        let record = store
            .observation_model_record(&exchange)
            .expect("record")
            .expect("present");
        assert_eq!(record.confirmed_sequence, 1);
        assert_eq!(record.response_bytes, b"first");

        let second = retain_observation_chunk(&mut store, &exchange, 2, 'b', b"second")
            .expect("second chunk");
        assert_eq!(
            second,
            ObservationChunkRetention::Inserted {
                confirmed_sequence: 2
            }
        );

        let tampered = retain_observation_chunk(&mut store, &exchange, 2, 'd', b"second")
            .expect_err("a different frame at a retained sequence");
        assert_eq!(tampered.message(), conflict().message());
    }

    #[test]
    fn acceptance_gates_the_exact_revision_and_releases_the_batch_once() {
        let (_directory, mut store) = open_store();
        let identity = claim(&mut store);
        let exchange = open_observation_exchange(&mut store, &identity);
        store
            .retain_observation_model_chunk(
                &observation_frame(&exchange, 1, 'a', b"frame", Some("completed")),
                &instant(),
            )
            .expect("terminal chunk");
        store
            .retain_observation_receipt(&accept_receipt(&observation_intent(&identity)), &instant())
            .expect("retain observation receipt");
        store
            .retain_workspace_progress(
                WORKSPACE,
                &progress_event(
                    &identity,
                    8,
                    ChangeBatchProgressState::ObservationCompleted,
                    "observation completed",
                ),
                BatchState::ObservationPending,
                BatchState::ObservationPending,
            )
            .expect("observation completed");
        let accepted = progress_event(&identity, 9, ChangeBatchProgressState::Accepted, "accepted");

        assert_eq!(
            store
                .accept_observed_checkpoint(
                    WORKSPACE,
                    &accepted,
                    &revision("git-tree:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
                    &digest('3'),
                    &instant(),
                )
                .expect("a foreign revision gates without an error"),
            ObservationGateResult::Stale
        );
        assert_eq!(
            store
                .accept_observed_checkpoint(
                    WORKSPACE,
                    &accepted,
                    &revision(RESULT),
                    &digest('4'),
                    &instant(),
                )
                .expect("a foreign delta gates without an error"),
            ObservationGateResult::Stale
        );
        assert_eq!(
            store
                .accept_observed_checkpoint(
                    WORKSPACE,
                    &accepted,
                    &revision(RESULT),
                    &digest('3'),
                    &instant(),
                )
                .expect("accept"),
            ObservationGateResult::Accepted
        );
        let binding = store
            .workspace_binding(WORKSPACE)
            .expect("binding")
            .expect("present");
        assert_eq!(binding.state, BatchState::Accepted);
        assert_eq!(binding.accepted_revision, revision(RESULT));
        assert_eq!(binding.active_batch_id, None);
        assert_eq!(binding.checkpoint_revision, None);

        let repeat = progress_event(
            &identity,
            10,
            ChangeBatchProgressState::Accepted,
            "accepted again",
        );
        assert_eq!(
            store
                .accept_observed_checkpoint(
                    WORKSPACE,
                    &repeat,
                    &revision(RESULT),
                    &digest('3'),
                    &instant(),
                )
                .expect("a repeat gates without an error"),
            ObservationGateResult::Stale
        );
        let binding = store
            .workspace_binding(WORKSPACE)
            .expect("binding")
            .expect("present");
        assert_eq!(binding.state, BatchState::Accepted);
        assert_eq!(binding.accepted_revision, revision(RESULT));
        assert_eq!(binding.active_batch_id, None);
    }

    #[test]
    fn restart_reloads_the_binding_progress_and_observation_state() {
        let (directory, mut store) = open_store();
        let identity = claim(&mut store);
        advance_to_validation_pending(&mut store, &identity);
        advance_to_observation_ready(&mut store, &identity);
        request_observation(&mut store, &identity);
        store
            .retain_observation_model_open(&identity.batch_id, &model_open(&identity), &instant())
            .expect("retain model open");
        let records_before = store
            .records_for_job(&identity.job_id)
            .expect("records")
            .len();

        drop(store);
        let reopened = ChangeBatchStore::open(directory.path()).expect("reopen store");
        let binding = reopened
            .workspace_binding(WORKSPACE)
            .expect("binding")
            .expect("present");
        assert_eq!(binding.state, BatchState::ObservationPending);
        assert_eq!(binding.active_batch_id, Some(identity.batch_id.clone()));
        assert_eq!(
            binding.checkpoint_revision.as_ref(),
            Some(&revision(RESULT))
        );
        assert_eq!(binding.checkpoint_delta_digest.as_ref(), Some(&digest('3')));

        let states: Vec<_> = reopened
            .progress_events(&identity.batch_id)
            .expect("progress")
            .into_iter()
            .map(|event| event.state)
            .collect();
        assert_eq!(
            states,
            vec![
                ChangeBatchProgressState::Proposed,
                ChangeBatchProgressState::Authorized,
                ChangeBatchProgressState::ApplyStarted,
                ChangeBatchProgressState::Applied,
                ChangeBatchProgressState::ValidationStarted,
                ChangeBatchProgressState::ValidationCompleted,
                ChangeBatchProgressState::ObservationRequested,
            ]
        );
        assert_eq!(
            reopened
                .records_for_job(&identity.job_id)
                .expect("records")
                .len(),
            records_before
        );

        let exchange = ModelExchangeId(EXCHANGE.to_owned());
        let record = reopened
            .observation_model_record(&exchange)
            .expect("record")
            .expect("present");
        assert_eq!(
            record.model_open.as_ref().expect("open").model_exchange_id,
            exchange
        );
        assert_eq!(
            reopened
                .pending_observation_model_open(&identity.job_id)
                .expect("pending"),
            Some(record)
        );
    }

    #[test]
    fn a_second_active_batch_is_refused_until_the_first_reaches_a_terminal_decision() {
        let (_directory, mut store) = open_store();
        let identity = claim(&mut store);
        let second = other_identity('7');
        assert_eq!(
            store
                .retain_claimed_intent(
                    WORKSPACE,
                    &proposal_event(&second),
                    &revision(BASE),
                    &digest('6'),
                    &instant(),
                )
                .expect_err("second active batch")
                .message(),
            conflict().message()
        );
        assert!(identity.batch_id != second.batch_id);
    }
}

#[cfg(test)]
mod delta_tests {
    use super::*;

    fn create_summary() -> AppliedFileSummary {
        AppliedFileSummary {
            after_sha256: Some(Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(b"fixture\n")
            ))),
            before_sha256: None,
            bytes_after: 8,
            bytes_before: 0,
            mode_after: Some("0644".to_owned()),
            mode_before: None,
            move_path: None,
            operation: AppliedFileOperation::Create,
            path: "delegated.txt".to_owned(),
        }
    }

    #[test]
    fn delta_digest_is_order_independent_and_content_bound() {
        let create = create_summary();
        let update = AppliedFileSummary {
            after_sha256: Some(Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(b"next\n")
            ))),
            before_sha256: Some(Sha256Digest(format!(
                "sha256:{:x}",
                Sha256::digest(b"old\n")
            ))),
            bytes_after: 5,
            bytes_before: 4,
            mode_after: Some("0644".to_owned()),
            mode_before: Some("0644".to_owned()),
            move_path: None,
            operation: AppliedFileOperation::Update,
            path: "src/lib.rs".to_owned(),
        };
        let forward = derive_delta_digest(&[create.clone(), update.clone()]).expect("derive");
        let reordered = derive_delta_digest(&[update, create]).expect("derive reordered");
        assert_eq!(forward, reordered);
        let lone =
            derive_delta_digest(std::slice::from_ref(&create_summary())).expect("derive one");
        assert_ne!(forward, lone);
        assert!(valid_digest(&forward));
    }

    #[test]
    fn delta_digest_rejects_non_portable_and_inconsistent_summaries() {
        let mut summary = create_summary();
        summary.path = "/absolute/delegated.txt".to_owned();
        assert!(derive_delta_digest(std::slice::from_ref(&summary)).is_err());
        let mut summary = create_summary();
        summary.path = "nested/../delegated.txt".to_owned();
        assert!(derive_delta_digest(std::slice::from_ref(&summary)).is_err());
        let mut summary = create_summary();
        summary.bytes_before = 4;
        assert!(derive_delta_digest(std::slice::from_ref(&summary)).is_err());
        let mut summary = create_summary();
        summary.move_path = Some("other.txt".to_owned());
        assert!(derive_delta_digest(std::slice::from_ref(&summary)).is_err());
    }
}
