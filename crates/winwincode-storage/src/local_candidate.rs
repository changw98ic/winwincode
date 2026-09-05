// SPDX-License-Identifier: Apache-2.0

//! Durable Server-side `LocalCandidateReceipt` ledger and the append-only
//! `LocalApplyReceipt` history.
//!
//! The Device Client owns the local git facts of candidate delivery (plan 5.6,
//! contracts 6 and 8): it freezes a candidate commit under the stable ref
//! `refs/winwincode/candidates/<candidate-id>` before worktree cleanup and
//! reports `client.candidate.retained`; every later "create branch / apply /
//! discard" attempt answers with `client.candidate.apply_result`. The Control
//! Plane is the audit authority over those client-issued receipts: this ledger
//! stores the safe projection of the local ref state — ref name, commit, and
//! lifecycle state — bound precisely to the Candidate product identity
//! (receipt id, client node, repository binding, candidate ref), never an
//! absolute filesystem path.
//!
//! Semantics frozen by `docs/contracts/client-control-state-machines.md`
//! contract 6 and 8 and `schema/winwincode/v1/client-control.schema.json`:
//!
//! - Candidate retention is idempotent. A replay of the same receipt, or a
//!   duplicate report of an already retained candidate ref with identical
//!   facts, returns the original row; any field disagreement fails closed.
//! - Every apply attempt appends exactly one `LocalApplyReceipt` row. The
//!   rows are immutable: the module never updates or deletes them and
//!   `BEFORE UPDATE/DELETE` triggers abort any stray write at the database
//!   layer, so a retry appends a new receipt instead of rewriting history.
//! - Failure outcomes use the frozen `LocalApplyResult` result codes
//!   (`base_stale`, `working_tree_dirty`, `merge_conflict`,
//!   `candidate_missing`, `permission_denied`, `failed`); they map to the
//!   candidate state `failed`, which stays retryable.
//! - `applied` and `discarded` are terminal: no further apply result can be
//!   appended once the candidate lifecycle ended.
//! - A successful `applied` result must carry the `resulting_commit`; a
//!   failure or a discard must not claim one, and only a `merge_conflict`
//!   may carry a conflict artifact reference.

use std::fmt;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use winwincode_domain::Instant;

use crate::{SqliteStorage, StorageError};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_ID_BYTES: usize = 96;
const MAX_REF_BYTES: usize = 255;
const MAX_ARTIFACT_REF_BYTES: usize = 200;

const LOCAL_CANDIDATE_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS local_candidate_receipts (
    local_candidate_receipt_id TEXT PRIMARY KEY NOT NULL,
    client_node_id TEXT NOT NULL,
    repository_binding_id TEXT NOT NULL,
    candidate_ref TEXT NOT NULL,
    candidate_commit TEXT NOT NULL,
    local_ref_name TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN (
        'retained', 'branch_created', 'applied', 'discarded', 'failed')),
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0 AND revision <= 9007199254740991),
    FOREIGN KEY (client_node_id) REFERENCES client_nodes(client_node_id) ON DELETE RESTRICT,
    FOREIGN KEY (repository_binding_id)
        REFERENCES repository_bindings(repository_binding_id) ON DELETE RESTRICT
);
CREATE UNIQUE INDEX IF NOT EXISTS local_candidate_receipts_one_per_client_candidate
    ON local_candidate_receipts (client_node_id, candidate_ref);
CREATE INDEX IF NOT EXISTS local_candidate_receipts_by_binding
    ON local_candidate_receipts (client_node_id, repository_binding_id, state);
CREATE TABLE IF NOT EXISTS local_apply_receipts (
    local_apply_receipt_id TEXT PRIMARY KEY NOT NULL,
    local_candidate_receipt_id TEXT NOT NULL,
    client_node_id TEXT NOT NULL,
    repository_binding_id TEXT NOT NULL,
    candidate_ref TEXT NOT NULL,
    target_branch TEXT NOT NULL,
    expected_head TEXT NOT NULL,
    strategy TEXT NOT NULL CHECK (strategy IN (
        'create_branch', 'fast_forward', 'cherry_pick', 'merge')),
    result TEXT NOT NULL CHECK (result IN (
        'retained', 'branch_created', 'applied', 'base_stale',
        'working_tree_dirty', 'merge_conflict', 'candidate_missing',
        'permission_denied', 'discarded', 'failed')),
    resulting_commit TEXT,
    conflict_artifact_ref TEXT,
    created_at TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision = 1),
    FOREIGN KEY (local_candidate_receipt_id)
        REFERENCES local_candidate_receipts(local_candidate_receipt_id) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS local_apply_receipts_by_candidate
    ON local_apply_receipts (local_candidate_receipt_id, created_at, local_apply_receipt_id);
CREATE TRIGGER IF NOT EXISTS local_apply_receipts_no_update
    BEFORE UPDATE ON local_apply_receipts BEGIN
        SELECT RAISE(ABORT, 'local apply receipts are immutable');
    END;
CREATE TRIGGER IF NOT EXISTS local_apply_receipts_no_delete
    BEFORE DELETE ON local_apply_receipts BEGIN
        SELECT RAISE(ABORT, 'local apply receipts are immutable');
    END;
";

/// Retention lifecycle of one locally frozen candidate (contract 6).
///
/// `applied` and `discarded` are terminal. `failed` is deliberately not
/// terminal: every retry appends a fresh `LocalApplyReceipt` and may still
/// reach `applied`, `branch_created`, or `discarded`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCandidateReceiptState {
    /// The candidate commit is frozen under a stable local ref.
    Retained,
    /// A durable local branch was created from the candidate ref.
    BranchCreated,
    /// Terminal: the candidate reached its target branch.
    Applied,
    /// Terminal: the candidate was discarded.
    Discarded,
    /// The last apply attempt failed; retrying stays allowed.
    Failed,
}

impl LocalCandidateReceiptState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retained => "retained",
            Self::BranchCreated => "branch_created",
            Self::Applied => "applied",
            Self::Discarded => "discarded",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, LocalCandidateStoreError> {
        match value {
            "retained" => Ok(Self::Retained),
            "branch_created" => Ok(Self::BranchCreated),
            "applied" => Ok(Self::Applied),
            "discarded" => Ok(Self::Discarded),
            "failed" => Ok(Self::Failed),
            _ => Err(error(
                LocalCandidateStoreErrorKind::CorruptState,
                "stored local candidate receipt state is invalid",
            )),
        }
    }

    /// True once the candidate lifecycle ended; no further apply result may
    /// be appended for a terminal candidate.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Applied | Self::Discarded)
    }
}

impl fmt::Display for LocalCandidateReceiptState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Strategy the Device Client used (or attempted) to deliver the candidate
/// onto its target branch (contract 8).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalApplyStrategy {
    /// Create a durable local branch from the candidate ref.
    CreateBranch,
    /// Fast-forward the target branch to the candidate commit.
    FastForward,
    /// Cherry-pick the candidate commit onto the target branch.
    CherryPick,
    /// Merge the candidate into the target branch.
    Merge,
}

impl LocalApplyStrategy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CreateBranch => "create_branch",
            Self::FastForward => "fast_forward",
            Self::CherryPick => "cherry_pick",
            Self::Merge => "merge",
        }
    }

    fn parse(value: &str) -> Result<Self, LocalCandidateStoreError> {
        match value {
            "create_branch" => Ok(Self::CreateBranch),
            "fast_forward" => Ok(Self::FastForward),
            "cherry_pick" => Ok(Self::CherryPick),
            "merge" => Ok(Self::Merge),
            _ => Err(error(
                LocalCandidateStoreErrorKind::CorruptState,
                "stored local apply strategy is invalid",
            )),
        }
    }
}

impl fmt::Display for LocalApplyStrategy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Frozen terminal outcome of one local apply attempt (contract 8). Every
/// attempt writes exactly one receipt carrying exactly one result code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalApplyResult {
    /// The attempt left the candidate retained: no branch, no apply.
    Retained,
    /// The local branch was created.
    BranchCreated,
    /// The candidate reached its target branch.
    Applied,
    /// Fail closed: the target HEAD moved away from `expected_head`.
    BaseStale,
    /// Fail closed: the target work tree violates the dirty policy.
    WorkingTreeDirty,
    /// Fail closed: an isolated integration worktree reported a conflict.
    MergeConflict,
    /// Fail closed: the candidate ref no longer resolves locally.
    CandidateMissing,
    /// Fail closed: the user lacks the repository permission.
    PermissionDenied,
    /// The candidate was discarded.
    Discarded,
    /// Any other execution failure.
    Failed,
}

impl LocalApplyResult {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retained => "retained",
            Self::BranchCreated => "branch_created",
            Self::Applied => "applied",
            Self::BaseStale => "base_stale",
            Self::WorkingTreeDirty => "working_tree_dirty",
            Self::MergeConflict => "merge_conflict",
            Self::CandidateMissing => "candidate_missing",
            Self::PermissionDenied => "permission_denied",
            Self::Discarded => "discarded",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, LocalCandidateStoreError> {
        match value {
            "retained" => Ok(Self::Retained),
            "branch_created" => Ok(Self::BranchCreated),
            "applied" => Ok(Self::Applied),
            "base_stale" => Ok(Self::BaseStale),
            "working_tree_dirty" => Ok(Self::WorkingTreeDirty),
            "merge_conflict" => Ok(Self::MergeConflict),
            "candidate_missing" => Ok(Self::CandidateMissing),
            "permission_denied" => Ok(Self::PermissionDenied),
            "discarded" => Ok(Self::Discarded),
            "failed" => Ok(Self::Failed),
            _ => Err(error(
                LocalCandidateStoreErrorKind::CorruptState,
                "stored local apply result is invalid",
            )),
        }
    }

    /// The candidate state this result projects onto (contract 8 result-to-
    /// state table). Failure codes all map to `failed`.
    #[must_use]
    pub const fn candidate_state(self) -> LocalCandidateReceiptState {
        match self {
            Self::Retained => LocalCandidateReceiptState::Retained,
            Self::BranchCreated => LocalCandidateReceiptState::BranchCreated,
            Self::Applied => LocalCandidateReceiptState::Applied,
            Self::BaseStale
            | Self::WorkingTreeDirty
            | Self::MergeConflict
            | Self::CandidateMissing
            | Self::PermissionDenied
            | Self::Failed => LocalCandidateReceiptState::Failed,
            Self::Discarded => LocalCandidateReceiptState::Discarded,
        }
    }

    /// True for the fail-closed result codes; they never produced a commit
    /// and always stay retryable.
    #[must_use]
    pub const fn is_failure(self) -> bool {
        matches!(
            self,
            Self::BaseStale
                | Self::WorkingTreeDirty
                | Self::MergeConflict
                | Self::CandidateMissing
                | Self::PermissionDenied
                | Self::Failed
        )
    }
}

impl fmt::Display for LocalApplyResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Validated `client.candidate.retained` append command.
///
/// The `_id` and `_ref` postfixes on several fields are the frozen
/// client-control schema vocabulary, so the lint against repeated field
/// suffixes is intentionally allowed here.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCandidateRetained {
    local_candidate_receipt_id: String,
    client_node_id: String,
    repository_binding_id: String,
    candidate_ref: String,
    candidate_commit: String,
    local_ref_name: String,
}

impl LocalCandidateRetained {
    /// Builds one validated retention command.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical receipt, client node, and binding identities, a
    /// candidate ref outside `refs/winwincode/candidates/`, a non-hex commit,
    /// or a non-canonical local ref name before any durable write.
    pub fn try_new(
        local_candidate_receipt_id: impl Into<String>,
        client_node_id: impl Into<String>,
        repository_binding_id: impl Into<String>,
        candidate_ref: impl Into<String>,
        candidate_commit: impl Into<String>,
        local_ref_name: impl Into<String>,
    ) -> Result<Self, LocalCandidateStoreError> {
        let retained = Self {
            local_candidate_receipt_id: local_candidate_receipt_id.into(),
            client_node_id: client_node_id.into(),
            repository_binding_id: repository_binding_id.into(),
            candidate_ref: candidate_ref.into(),
            candidate_commit: candidate_commit.into(),
            local_ref_name: local_ref_name.into(),
        };
        validate_candidate_receipt_id(&retained.local_candidate_receipt_id)?;
        validate_client_node_id(&retained.client_node_id)?;
        validate_repository_binding_id(&retained.repository_binding_id)?;
        validate_candidate_ref(&retained.candidate_ref)?;
        validate_commit_sha(&retained.candidate_commit, "candidate commit")?;
        validate_git_ref_name(&retained.local_ref_name, "local ref name")?;
        Ok(retained)
    }

    #[must_use]
    pub fn local_candidate_receipt_id(&self) -> &str {
        &self.local_candidate_receipt_id
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn repository_binding_id(&self) -> &str {
        &self.repository_binding_id
    }

    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub fn candidate_commit(&self) -> &str {
        &self.candidate_commit
    }

    #[must_use]
    pub fn local_ref_name(&self) -> &str {
        &self.local_ref_name
    }
}

/// Validated `client.candidate.apply_result` settlement command: the
/// immutable record of exactly one local apply attempt.
#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalApplySettlement {
    local_apply_receipt_id: String,
    local_candidate_receipt_id: String,
    client_node_id: String,
    repository_binding_id: String,
    candidate_ref: String,
    target_branch: String,
    expected_head: String,
    strategy: LocalApplyStrategy,
    result: LocalApplyResult,
    resulting_commit: Option<String>,
    conflict_artifact_ref: Option<String>,
}

impl LocalApplySettlement {
    /// Builds one validated apply-result settlement.
    ///
    /// # Errors
    ///
    /// Rejects non-canonical identities, refs, commits, and artifact
    /// references, an `applied` result without a resulting commit, a
    /// resulting commit on any non-applying outcome, or a conflict artifact
    /// reference outside a `merge_conflict` result.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        local_apply_receipt_id: impl Into<String>,
        local_candidate_receipt_id: impl Into<String>,
        client_node_id: impl Into<String>,
        repository_binding_id: impl Into<String>,
        candidate_ref: impl Into<String>,
        target_branch: impl Into<String>,
        expected_head: impl Into<String>,
        strategy: LocalApplyStrategy,
        result: LocalApplyResult,
        resulting_commit: Option<String>,
        conflict_artifact_ref: Option<String>,
    ) -> Result<Self, LocalCandidateStoreError> {
        let settlement = Self {
            local_apply_receipt_id: local_apply_receipt_id.into(),
            local_candidate_receipt_id: local_candidate_receipt_id.into(),
            client_node_id: client_node_id.into(),
            repository_binding_id: repository_binding_id.into(),
            candidate_ref: candidate_ref.into(),
            target_branch: target_branch.into(),
            expected_head: expected_head.into(),
            strategy,
            result,
            resulting_commit,
            conflict_artifact_ref,
        };
        validate_apply_receipt_id(&settlement.local_apply_receipt_id)?;
        validate_candidate_receipt_id(&settlement.local_candidate_receipt_id)?;
        validate_client_node_id(&settlement.client_node_id)?;
        validate_repository_binding_id(&settlement.repository_binding_id)?;
        validate_candidate_ref(&settlement.candidate_ref)?;
        validate_git_ref_name(&settlement.target_branch, "target branch")?;
        validate_commit_sha(&settlement.expected_head, "expected head")?;
        if let Some(resulting) = settlement.resulting_commit.as_deref() {
            validate_commit_sha(resulting, "resulting commit")?;
        }
        if let Some(artifact) = settlement.conflict_artifact_ref.as_deref() {
            validate_conflict_artifact_ref(artifact)?;
        }
        match settlement.result {
            LocalApplyResult::Applied => {
                if settlement.resulting_commit.is_none() {
                    return Err(error(
                        LocalCandidateStoreErrorKind::InvalidInput,
                        "an applied result must carry its resulting commit",
                    ));
                }
            }
            LocalApplyResult::BranchCreated => {}
            LocalApplyResult::Retained
            | LocalApplyResult::BaseStale
            | LocalApplyResult::WorkingTreeDirty
            | LocalApplyResult::MergeConflict
            | LocalApplyResult::CandidateMissing
            | LocalApplyResult::PermissionDenied
            | LocalApplyResult::Discarded
            | LocalApplyResult::Failed => {
                if settlement.resulting_commit.is_some() {
                    return Err(error(
                        LocalCandidateStoreErrorKind::InvalidInput,
                        "a result that produced no commit must not claim one",
                    ));
                }
            }
        }
        if settlement.conflict_artifact_ref.is_some()
            && settlement.result != LocalApplyResult::MergeConflict
        {
            return Err(error(
                LocalCandidateStoreErrorKind::InvalidInput,
                "only a merge conflict result may carry a conflict artifact reference",
            ));
        }
        Ok(settlement)
    }

    #[must_use]
    pub fn local_apply_receipt_id(&self) -> &str {
        &self.local_apply_receipt_id
    }

    #[must_use]
    pub fn local_candidate_receipt_id(&self) -> &str {
        &self.local_candidate_receipt_id
    }

    #[must_use]
    pub fn client_node_id(&self) -> &str {
        &self.client_node_id
    }

    #[must_use]
    pub fn repository_binding_id(&self) -> &str {
        &self.repository_binding_id
    }

    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub fn target_branch(&self) -> &str {
        &self.target_branch
    }

    #[must_use]
    pub fn expected_head(&self) -> &str {
        &self.expected_head
    }

    #[must_use]
    pub const fn strategy(&self) -> LocalApplyStrategy {
        self.strategy
    }

    #[must_use]
    pub const fn result(&self) -> LocalApplyResult {
        self.result
    }

    #[must_use]
    pub fn resulting_commit(&self) -> Option<&str> {
        self.resulting_commit.as_deref()
    }

    #[must_use]
    pub fn conflict_artifact_ref(&self) -> Option<&str> {
        self.conflict_artifact_ref.as_deref()
    }
}

/// Durable `LocalCandidateReceipt` row: the safe projection of one locally
/// frozen candidate (plan 5.6, contract 6).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCandidateReceiptRecord {
    /// Stable client-issued retention receipt identifier.
    pub local_candidate_receipt_id: String,
    /// The client node whose device owns the local ref.
    pub client_node_id: String,
    /// Repository binding the candidate was produced against.
    pub repository_binding_id: String,
    /// Stable local git ref, `refs/winwincode/candidates/<candidate-id>`.
    pub candidate_ref: String,
    /// Frozen candidate commit (full SHA-1 or SHA-256).
    pub candidate_commit: String,
    /// Local ref name as reported by the Device Client.
    pub local_ref_name: String,
    /// Retention lifecycle state.
    pub state: LocalCandidateReceiptState,
    /// Instant the retention was first recorded.
    pub created_at: Instant,
    /// Monotonic optimistic-concurrency revision.
    pub revision: u64,
}

/// Durable `LocalApplyReceipt` row: the immutable audit record of exactly
/// one local apply attempt (contract 8).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalApplyReceiptRecord {
    /// Stable client-issued apply receipt identifier.
    pub local_apply_receipt_id: String,
    /// The candidate receipt this attempt settled against.
    pub local_candidate_receipt_id: String,
    /// The client node that executed the attempt.
    pub client_node_id: String,
    /// Repository binding the attempt executed against.
    pub repository_binding_id: String,
    /// Candidate ref the attempt targeted.
    pub candidate_ref: String,
    /// Target branch of the attempt.
    pub target_branch: String,
    /// Expected target HEAD the attempt was validated against.
    pub expected_head: String,
    /// Delivery strategy the attempt used.
    pub strategy: LocalApplyStrategy,
    /// Frozen terminal outcome.
    pub result: LocalApplyResult,
    /// Commit produced by the attempt; only `applied` (and optionally
    /// `branch_created`) carry one.
    pub resulting_commit: Option<String>,
    /// Opaque client-local conflict artifact reference; only a
    /// `merge_conflict` result carries one, never a filesystem path.
    pub conflict_artifact_ref: Option<String>,
    /// Instant the attempt was settled.
    pub created_at: Instant,
    /// Fixed at one: the row is immutable after its single insert.
    pub revision: u64,
}

/// Stable local-candidate ledger failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCandidateStoreErrorKind {
    /// A command input violated the frozen schema bounds.
    InvalidInput,
    /// The client node identity does not exist.
    UnknownClientNode,
    /// No repository binding matches the requested identity.
    UnknownRepositoryBinding,
    /// No local candidate receipt matches the requested identity.
    UnknownLocalCandidate,
    /// No local apply receipt matches the requested identity.
    UnknownLocalApplyReceipt,
    /// The candidate is already retained under a different identity or with
    /// different facts.
    LocalCandidateConflict,
    /// The apply receipt id is already used with different fields.
    ApplyReceiptConflict,
    /// The candidate already reached its `applied` or `discarded` terminal.
    TerminalCandidateConflict,
    /// The settlement identity does not match the retained candidate.
    CandidateIdentityMismatch,
    /// The requested change is not a legal state machine transition.
    IllegalStateTransition,
    /// A compare-and-swap guard lost a race that should be impossible inside
    /// one immediate transaction.
    RevisionConflict,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free local-candidate ledger error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCandidateStoreError {
    kind: LocalCandidateStoreErrorKind,
    message: String,
}

impl LocalCandidateStoreError {
    #[must_use]
    pub const fn kind(&self) -> LocalCandidateStoreErrorKind {
        self.kind
    }
}

impl fmt::Display for LocalCandidateStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LocalCandidateStoreError {}

/// Local candidate and apply receipt ledger borrowing the sole product-state
/// `SQLite` authority.
pub struct LocalCandidateLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the durable local candidate ledger on this same product-state
    /// database.
    ///
    /// # Errors
    ///
    /// Rejects an unavailable connection or an incompatible existing schema.
    pub fn local_candidate_ledger(
        &mut self,
    ) -> Result<LocalCandidateLedger<'_>, LocalCandidateStoreError> {
        LocalCandidateLedger::new(self)
    }
}

impl<'storage> LocalCandidateLedger<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, LocalCandidateStoreError> {
        let connection = storage
            .connection()
            .map_err(|storage| storage_error(&storage))?;
        connection
            .execute_batch(LOCAL_CANDIDATE_SCHEMA)
            .map_err(|sql| sql_error(&sql))?;
        validate_schema(connection)?;
        Ok(Self { storage })
    }

    /// Records a `client.candidate.retained` report (plan 5.6, contract 6):
    /// the candidate commit is frozen under its stable local ref and the
    /// receipt enters the ledger as `retained`.
    ///
    /// The append is idempotent in both replay shapes: the same receipt id
    /// replayed with identical facts returns the original row, and a
    /// duplicate retention of an already retained candidate ref (even under
    /// a fresh receipt id) also returns the original row. Any field
    /// disagreement fails closed with a stable conflict category; the
    /// candidate identity `(client_node_id, candidate_ref)` is enforced
    /// durably by a unique index.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical command, an unknown client node or repository
    /// binding, a candidate ref already retained with different facts, a
    /// receipt id reused for a different candidate, or storage failure.
    pub fn record_retained(
        &mut self,
        retained: &LocalCandidateRetained,
        now: &Instant,
    ) -> Result<LocalCandidateReceiptRecord, LocalCandidateStoreError> {
        validate_instant(now, "retention time")?;
        let transaction = self.transaction()?;
        if let Some(existing) = load_candidate(&transaction, retained.local_candidate_receipt_id())?
        {
            ensure_candidate_identity(&existing, retained)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(existing);
        }
        if let Some(existing) = load_candidate_by_ref(
            &transaction,
            retained.client_node_id(),
            retained.candidate_ref(),
        )? {
            ensure_candidate_facts(&existing, retained)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok(existing);
        }
        ensure_retention_owners(&transaction, retained)?;
        let inserted = transaction
            .execute(
                "INSERT INTO local_candidate_receipts
                 (local_candidate_receipt_id, client_node_id, repository_binding_id,
                  candidate_ref, candidate_commit, local_ref_name, state,
                  created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'retained', ?7, 1)",
                params![
                    retained.local_candidate_receipt_id(),
                    retained.client_node_id(),
                    retained.repository_binding_id(),
                    retained.candidate_ref(),
                    retained.candidate_commit(),
                    retained.local_ref_name(),
                    now.0,
                ],
            )
            .map_err(|sql| map_candidate_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                LocalCandidateStoreErrorKind::Storage,
                "local candidate receipt insert did not store exactly one row",
            ));
        }
        let record = load_candidate(&transaction, retained.local_candidate_receipt_id())?
            .ok_or_else(candidate_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok(record)
    }

    /// Records a `client.candidate.apply_result` settlement (contract 8) and
    /// advances the candidate state machine in the same transaction.
    ///
    /// The attempt identity must match the retained candidate exactly
    /// (client node, repository binding, and candidate ref). Every accepted
    /// settlement appends exactly one immutable `LocalApplyReceipt` row; the
    /// frozen result code decides the candidate state projection and
    /// failure codes keep the candidate retryable. A replay of an already
    /// settled receipt id with identical fields is an accepted idempotent
    /// no-op that returns the stored rows unchanged. `applied` and
    /// `discarded` candidates refuse any further settlement.
    ///
    /// Returns the immutable apply receipt plus the candidate receipt in its
    /// post-settlement state.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical command, an unknown candidate, an identity
    /// mismatch against the retained candidate, an apply receipt id reused
    /// with different fields, a terminal candidate, a result that is not a
    /// legal projection of the candidate's current state, or storage
    /// failure.
    pub fn record_apply_result(
        &mut self,
        settlement: &LocalApplySettlement,
        now: &Instant,
    ) -> Result<(LocalApplyReceiptRecord, LocalCandidateReceiptRecord), LocalCandidateStoreError>
    {
        validate_instant(now, "apply result time")?;
        let transaction = self.transaction()?;
        let candidate = load_candidate(&transaction, settlement.local_candidate_receipt_id())?
            .ok_or_else(|| {
                error(
                    LocalCandidateStoreErrorKind::UnknownLocalCandidate,
                    "local candidate receipt does not exist",
                )
            })?;
        ensure_settlement_binding(&candidate, settlement)?;
        if let Some(existing) = load_apply(&transaction, settlement.local_apply_receipt_id())? {
            ensure_settlement_fields(&existing, settlement)?;
            transaction.commit().map_err(|sql| sql_error(&sql))?;
            return Ok((existing, candidate));
        }
        if candidate.state.is_terminal() {
            return Err(error(
                LocalCandidateStoreErrorKind::TerminalCandidateConflict,
                format!(
                    "local candidate already reached the terminal state {}",
                    candidate.state
                ),
            ));
        }
        let target = settlement.result().candidate_state();
        ensure_legal_projection(&candidate, settlement.result())?;
        let inserted = transaction
            .execute(
                "INSERT INTO local_apply_receipts
                 (local_apply_receipt_id, local_candidate_receipt_id, client_node_id,
                  repository_binding_id, candidate_ref, target_branch, expected_head,
                  strategy, result, resulting_commit, conflict_artifact_ref,
                  created_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1)",
                params![
                    settlement.local_apply_receipt_id(),
                    settlement.local_candidate_receipt_id(),
                    settlement.client_node_id(),
                    settlement.repository_binding_id(),
                    settlement.candidate_ref(),
                    settlement.target_branch(),
                    settlement.expected_head(),
                    settlement.strategy().as_str(),
                    settlement.result().as_str(),
                    settlement.resulting_commit().map(str::to_owned),
                    settlement.conflict_artifact_ref().map(str::to_owned),
                    now.0,
                ],
            )
            .map_err(|sql| map_apply_insert_sql(&sql))?;
        if inserted != 1 {
            return Err(error(
                LocalCandidateStoreErrorKind::Storage,
                "local apply receipt insert did not store exactly one row",
            ));
        }
        if target != candidate.state {
            let updated = transaction
                .execute(
                    "UPDATE local_candidate_receipts
                     SET state = ?2, revision = revision + 1
                     WHERE local_candidate_receipt_id = ?1 AND state = ?3",
                    params![
                        candidate.local_candidate_receipt_id,
                        target.as_str(),
                        candidate.state.as_str()
                    ],
                )
                .map_err(|sql| sql_error(&sql))?;
            if updated != 1 {
                return Err(cas_lost("apply result settlement"));
            }
        }
        let apply_receipt = load_apply(&transaction, settlement.local_apply_receipt_id())?
            .ok_or_else(apply_missing_after_write)?;
        let candidate =
            load_candidate(&transaction, candidate.local_candidate_receipt_id.as_str())?
                .ok_or_else(candidate_missing_after_write)?;
        transaction.commit().map_err(|sql| sql_error(&sql))?;
        Ok((apply_receipt, candidate))
    }

    /// Returns one durable local candidate receipt projection.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical receipt identity, corrupt stored rows, or
    /// storage failure.
    pub fn candidate_snapshot(
        &self,
        local_candidate_receipt_id: &str,
    ) -> Result<Option<LocalCandidateReceiptRecord>, LocalCandidateStoreError> {
        validate_candidate_receipt_id(local_candidate_receipt_id)?;
        load_candidate(self.connection()?, local_candidate_receipt_id)
    }

    /// Returns the candidate receipt for one client-local candidate ref, if
    /// any. This is the lookup the Device Client exchange identities use.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical client node identity or candidate ref, or
    /// storage failure.
    pub fn candidate_for_ref(
        &self,
        client_node_id: &str,
        candidate_ref: &str,
    ) -> Result<Option<LocalCandidateReceiptRecord>, LocalCandidateStoreError> {
        validate_client_node_id(client_node_id)?;
        validate_candidate_ref(candidate_ref)?;
        load_candidate_by_ref(self.connection()?, client_node_id, candidate_ref)
    }

    /// Returns one immutable local apply receipt.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical receipt identity or storage failure.
    pub fn apply_receipt(
        &self,
        local_apply_receipt_id: &str,
    ) -> Result<Option<LocalApplyReceiptRecord>, LocalCandidateStoreError> {
        validate_apply_receipt_id(local_apply_receipt_id)?;
        load_apply(self.connection()?, local_apply_receipt_id)
    }

    /// Returns the full immutable apply history of one candidate, oldest
    /// first, ordered deterministically by settlement instant then receipt
    /// id. History rows are never rewritten by later settlements.
    ///
    /// # Errors
    ///
    /// Rejects a non-canonical receipt identity or storage failure.
    pub fn apply_history_for_candidate(
        &self,
        local_candidate_receipt_id: &str,
    ) -> Result<Vec<LocalApplyReceiptRecord>, LocalCandidateStoreError> {
        validate_candidate_receipt_id(local_candidate_receipt_id)?;
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT local_apply_receipt_id, local_candidate_receipt_id, client_node_id,
                        repository_binding_id, candidate_ref, target_branch, expected_head,
                        strategy, result, resulting_commit, conflict_artifact_ref,
                        created_at, revision
                 FROM local_apply_receipts
                 WHERE local_candidate_receipt_id = ?1
                 ORDER BY created_at, local_apply_receipt_id",
            )
            .map_err(|sql| sql_error(&sql))?;
        let rows = statement
            .query_map([local_candidate_receipt_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            })
            .map_err(|sql| sql_error(&sql))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|sql| sql_error(&sql))?;
        rows.into_iter().map(apply_receipt_from_row).collect()
    }

    fn transaction(&mut self) -> Result<Transaction<'_>, LocalCandidateStoreError> {
        self.storage
            .connection_mut()
            .map_err(|storage| storage_error(&storage))?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|sql| sql_error(&sql))
    }

    fn connection(&self) -> Result<&rusqlite::Connection, LocalCandidateStoreError> {
        self.storage
            .connection()
            .map_err(|storage| storage_error(&storage))
    }
}

/// Rejects an apply result whose projection is not legal from the
/// candidate's current state (contract 6 legal transition table).
fn ensure_legal_projection(
    candidate: &LocalCandidateReceiptRecord,
    result: LocalApplyResult,
) -> Result<(), LocalCandidateStoreError> {
    let legal = match result {
        // A retained outcome confirms the candidate is still merely
        // retained; no other state projects back to `retained`.
        LocalApplyResult::Retained => candidate.state == LocalCandidateReceiptState::Retained,
        // Branch creation, application, discarding, and every failure code
        // stay reachable from every non-terminal state — contract 6 keeps
        // `failed` retryable up to `applied` / `branch_created` /
        // `discarded`. Terminal states were already refused above.
        _ => !candidate.state.is_terminal(),
    };
    if legal {
        return Ok(());
    }
    Err(error(
        LocalCandidateStoreErrorKind::IllegalStateTransition,
        format!(
            "local candidate transition {} -> {} during apply result is not legal",
            candidate.state,
            result.candidate_state()
        ),
    ))
}

/// Judges the retention owners inside the caller's transaction so an unknown
/// identity reports precisely instead of masking as a foreign-key failure.
/// The durable foreign keys remain the race backstop.
fn ensure_retention_owners(
    transaction: &Transaction<'_>,
    retained: &LocalCandidateRetained,
) -> Result<(), LocalCandidateStoreError> {
    let node: Option<String> = transaction
        .query_row(
            "SELECT client_node_id FROM client_nodes WHERE client_node_id = ?1",
            [retained.client_node_id()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    if node.is_none() {
        return Err(unknown_client_node());
    }
    let binding: Option<i64> = transaction
        .query_row(
            "SELECT 1 FROM repository_bindings WHERE repository_binding_id = ?1",
            [retained.repository_binding_id()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?;
    if binding.is_none() {
        return Err(unknown_repository_binding());
    }
    Ok(())
}

/// Ensures a replayed or duplicate retention matches the stored candidate
/// identity exactly.
fn ensure_candidate_identity(
    existing: &LocalCandidateReceiptRecord,
    retained: &LocalCandidateRetained,
) -> Result<(), LocalCandidateStoreError> {
    let matches = existing.client_node_id == retained.client_node_id()
        && existing.repository_binding_id == retained.repository_binding_id()
        && existing.candidate_ref == retained.candidate_ref()
        && existing.candidate_commit == retained.candidate_commit()
        && existing.local_ref_name == retained.local_ref_name();
    if matches {
        return Ok(());
    }
    Err(error(
        LocalCandidateStoreErrorKind::LocalCandidateConflict,
        "local candidate receipt id is already retained with different facts",
    ))
}

/// Ensures a duplicate retention under a fresh receipt id still describes
/// the same frozen candidate facts.
fn ensure_candidate_facts(
    existing: &LocalCandidateReceiptRecord,
    retained: &LocalCandidateRetained,
) -> Result<(), LocalCandidateStoreError> {
    let matches = existing.repository_binding_id == retained.repository_binding_id()
        && existing.candidate_commit == retained.candidate_commit()
        && existing.local_ref_name == retained.local_ref_name();
    if matches {
        return Ok(());
    }
    Err(error(
        LocalCandidateStoreErrorKind::LocalCandidateConflict,
        "candidate ref is already retained with different facts",
    ))
}

/// Ensures the settlement names exactly the retained candidate identity:
/// precise Candidate product binding (plan 5.6).
fn ensure_settlement_binding(
    candidate: &LocalCandidateReceiptRecord,
    settlement: &LocalApplySettlement,
) -> Result<(), LocalCandidateStoreError> {
    let matches = candidate.client_node_id == settlement.client_node_id()
        && candidate.repository_binding_id == settlement.repository_binding_id()
        && candidate.candidate_ref == settlement.candidate_ref();
    if matches {
        return Ok(());
    }
    Err(error(
        LocalCandidateStoreErrorKind::CandidateIdentityMismatch,
        "apply result identity does not match the retained candidate",
    ))
}

/// Ensures a replayed settlement matches the stored immutable receipt.
fn ensure_settlement_fields(
    existing: &LocalApplyReceiptRecord,
    settlement: &LocalApplySettlement,
) -> Result<(), LocalCandidateStoreError> {
    let matches = existing.local_apply_receipt_id == settlement.local_apply_receipt_id()
        && existing.local_candidate_receipt_id == settlement.local_candidate_receipt_id()
        && existing.client_node_id == settlement.client_node_id()
        && existing.repository_binding_id == settlement.repository_binding_id()
        && existing.candidate_ref == settlement.candidate_ref()
        && existing.target_branch == settlement.target_branch()
        && existing.expected_head == settlement.expected_head()
        && existing.strategy == settlement.strategy()
        && existing.result == settlement.result()
        && existing.resulting_commit == settlement.resulting_commit().map(str::to_owned)
        && existing.conflict_artifact_ref == settlement.conflict_artifact_ref().map(str::to_owned);
    if matches {
        return Ok(());
    }
    Err(error(
        LocalCandidateStoreErrorKind::ApplyReceiptConflict,
        "local apply receipt id is already settled with different fields",
    ))
}

fn load_candidate(
    connection: &rusqlite::Connection,
    local_candidate_receipt_id: &str,
) -> Result<Option<LocalCandidateReceiptRecord>, LocalCandidateStoreError> {
    connection
        .query_row(
            "SELECT local_candidate_receipt_id, client_node_id, repository_binding_id,
                    candidate_ref, candidate_commit, local_ref_name, state,
                    created_at, revision
             FROM local_candidate_receipts WHERE local_candidate_receipt_id = ?1",
            [local_candidate_receipt_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(candidate_record_from_row)
        .transpose()
}

fn load_candidate_by_ref(
    connection: &rusqlite::Connection,
    client_node_id: &str,
    candidate_ref: &str,
) -> Result<Option<LocalCandidateReceiptRecord>, LocalCandidateStoreError> {
    connection
        .query_row(
            "SELECT local_candidate_receipt_id, client_node_id, repository_binding_id,
                    candidate_ref, candidate_commit, local_ref_name, state,
                    created_at, revision
             FROM local_candidate_receipts
             WHERE client_node_id = ?1 AND candidate_ref = ?2",
            params![client_node_id, candidate_ref],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(candidate_record_from_row)
        .transpose()
}

#[allow(clippy::type_complexity)]
fn candidate_record_from_row(
    row: (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
    ),
) -> Result<LocalCandidateReceiptRecord, LocalCandidateStoreError> {
    let (
        local_candidate_receipt_id,
        client_node_id,
        repository_binding_id,
        candidate_ref,
        candidate_commit,
        local_ref_name,
        state,
        created_at,
        revision,
    ) = row;
    Ok(LocalCandidateReceiptRecord {
        local_candidate_receipt_id,
        client_node_id,
        repository_binding_id,
        candidate_ref,
        candidate_commit,
        local_ref_name,
        state: LocalCandidateReceiptState::parse(&state)?,
        created_at: parse_stored_instant(&created_at, "candidate created at")?,
        revision: from_sql_integer(revision, "local candidate receipt revision")?,
    })
}

#[allow(clippy::type_complexity)]
fn load_apply(
    connection: &rusqlite::Connection,
    local_apply_receipt_id: &str,
) -> Result<Option<LocalApplyReceiptRecord>, LocalCandidateStoreError> {
    connection
        .query_row(
            "SELECT local_apply_receipt_id, local_candidate_receipt_id, client_node_id,
                    repository_binding_id, candidate_ref, target_branch, expected_head,
                    strategy, result, resulting_commit, conflict_artifact_ref,
                    created_at, revision
             FROM local_apply_receipts WHERE local_apply_receipt_id = ?1",
            [local_apply_receipt_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            },
        )
        .optional()
        .map_err(|sql| sql_error(&sql))?
        .map(apply_receipt_from_row)
        .transpose()
}

#[allow(clippy::type_complexity)]
fn apply_receipt_from_row(
    row: (
        String,
        String,
        String,
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
    ),
) -> Result<LocalApplyReceiptRecord, LocalCandidateStoreError> {
    let (
        local_apply_receipt_id,
        local_candidate_receipt_id,
        client_node_id,
        repository_binding_id,
        candidate_ref,
        target_branch,
        expected_head,
        strategy,
        result,
        resulting_commit,
        conflict_artifact_ref,
        created_at,
        revision,
    ) = row;
    Ok(LocalApplyReceiptRecord {
        local_apply_receipt_id,
        local_candidate_receipt_id,
        client_node_id,
        repository_binding_id,
        candidate_ref,
        target_branch,
        expected_head,
        strategy: LocalApplyStrategy::parse(&strategy)?,
        result: LocalApplyResult::parse(&result)?,
        resulting_commit,
        conflict_artifact_ref,
        created_at: parse_stored_instant(&created_at, "apply receipt created at")?,
        revision: from_sql_integer(revision, "local apply receipt revision")?,
    })
}

fn validate_schema(connection: &rusqlite::Connection) -> Result<(), LocalCandidateStoreError> {
    validate_columns(
        connection,
        "local_candidate_receipts",
        &[
            "local_candidate_receipt_id",
            "client_node_id",
            "repository_binding_id",
            "candidate_ref",
            "candidate_commit",
            "local_ref_name",
            "state",
            "created_at",
            "revision",
        ],
    )?;
    validate_columns(
        connection,
        "local_apply_receipts",
        &[
            "local_apply_receipt_id",
            "local_candidate_receipt_id",
            "client_node_id",
            "repository_binding_id",
            "candidate_ref",
            "target_branch",
            "expected_head",
            "strategy",
            "result",
            "resulting_commit",
            "conflict_artifact_ref",
            "created_at",
            "revision",
        ],
    )
}

fn validate_columns(
    connection: &rusqlite::Connection,
    table: &str,
    expected: &[&str],
) -> Result<(), LocalCandidateStoreError> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut statement = connection.prepare(&pragma).map_err(|sql| sql_error(&sql))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|sql| sql_error(&sql))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|sql| sql_error(&sql))?;
    if columns != expected {
        return Err(error(
            LocalCandidateStoreErrorKind::CorruptState,
            "local candidate ledger schema is incompatible",
        ));
    }
    Ok(())
}

fn validate_candidate_receipt_id(value: &str) -> Result<(), LocalCandidateStoreError> {
    validate_crockford_id(value, "lcr_", "local candidate receipt id")
}

fn validate_apply_receipt_id(value: &str) -> Result<(), LocalCandidateStoreError> {
    validate_crockford_id(value, "lar_", "local apply receipt id")
}

fn validate_client_node_id(value: &str) -> Result<(), LocalCandidateStoreError> {
    validate_crockford_id(value, "cnd_", "client node id")
}

fn validate_repository_binding_id(value: &str) -> Result<(), LocalCandidateStoreError> {
    validate_crockford_id(value, "rbd_", "repository binding id")
}

fn validate_crockford_id(
    value: &str,
    prefix: &str,
    label: &str,
) -> Result<(), LocalCandidateStoreError> {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return Err(invalid(format!("{label} is not canonical")));
    };
    if suffix.len() != 26 || value.len() > MAX_ID_BYTES || !suffix.bytes().all(is_crockford_base32)
    {
        return Err(invalid(format!("{label} is not canonical")));
    }
    Ok(())
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

/// Validates the frozen `CandidateRef` shape:
/// `refs/winwincode/candidates/<candidate-id>`.
fn validate_candidate_ref(value: &str) -> Result<(), LocalCandidateStoreError> {
    const PREFIX: &str = "refs/winwincode/candidates/";
    let Some(suffix) = value.strip_prefix(PREFIX) else {
        return Err(invalid("candidate ref is not canonical"));
    };
    let valid = !suffix.is_empty()
        && suffix.len() <= 200
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && suffix.as_bytes()[0].is_ascii_alphanumeric();
    if valid {
        Ok(())
    } else {
        Err(invalid("candidate ref is not canonical"))
    }
}

/// Validates the frozen `GitCommitSha` shape: full SHA-1 or SHA-256, lower
/// case hex.
fn validate_commit_sha(value: &str, label: &str) -> Result<(), LocalCandidateStoreError> {
    let valid = (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(invalid(format!("{label} is not a full git commit name")))
    }
}

/// Validates the frozen `GitRefName` shape as accepted by
/// `git check-ref-format`; git itself enforces the full ref rules.
fn validate_git_ref_name(value: &str, label: &str) -> Result<(), LocalCandidateStoreError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_REF_BYTES
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(invalid(format!("{label} is not a canonical git ref name")))
    }
}

/// Validates the frozen conflict artifact reference shape: an opaque
/// client-local reference that can never be a filesystem path.
fn validate_conflict_artifact_ref(value: &str) -> Result<(), LocalCandidateStoreError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_ARTIFACT_REF_BYTES
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(invalid(
            "conflict artifact reference is not an opaque artifact reference",
        ))
    }
}

/// Validates the canonical `domain.Instant` shape (`YYYY-MM-DDTHH:MM:SS.sssZ`).
fn validate_instant(value: &Instant, label: &str) -> Result<(), LocalCandidateStoreError> {
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
        Err(invalid(format!("{label} instant is not canonical")))
    }
}

fn parse_stored_instant(value: &str, label: &str) -> Result<Instant, LocalCandidateStoreError> {
    let instant = Instant(value.to_owned());
    validate_instant(&instant, label).map(|()| instant)
}

fn map_candidate_insert_sql(sql: &rusqlite::Error) -> LocalCandidateStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            // The realistic unique violation is the one-identity-per-candidate
            // index; the foreign keys name their missing owner precisely.
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => error(
                LocalCandidateStoreErrorKind::LocalCandidateConflict,
                "candidate ref is already retained for this client node",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                LocalCandidateStoreErrorKind::LocalCandidateConflict,
                "local candidate receipt id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                unknown_binding_after_insert("local candidate receipt")
            }
            _ => error(
                LocalCandidateStoreErrorKind::InvalidInput,
                "local candidate receipt violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn map_apply_insert_sql(sql: &rusqlite::Error) -> LocalCandidateStoreError {
    if let rusqlite::Error::SqliteFailure(failure, _) = sql
        && failure.code == rusqlite::ErrorCode::ConstraintViolation
    {
        return match failure.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => error(
                LocalCandidateStoreErrorKind::ApplyReceiptConflict,
                "local apply receipt id is already used",
            ),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => {
                unknown_binding_after_insert("local apply receipt")
            }
            _ => error(
                LocalCandidateStoreErrorKind::InvalidInput,
                "local apply receipt violates a durable constraint",
            ),
        };
    }
    sql_error(sql)
}

fn unknown_binding_after_insert(row: &str) -> LocalCandidateStoreError {
    error(
        LocalCandidateStoreErrorKind::UnknownRepositoryBinding,
        format!("{row} names a client node or repository binding that does not exist"),
    )
}

fn unknown_client_node() -> LocalCandidateStoreError {
    error(
        LocalCandidateStoreErrorKind::UnknownClientNode,
        "client node does not exist",
    )
}

fn unknown_repository_binding() -> LocalCandidateStoreError {
    error(
        LocalCandidateStoreErrorKind::UnknownRepositoryBinding,
        "repository binding does not exist",
    )
}

fn candidate_missing_after_write() -> LocalCandidateStoreError {
    error(
        LocalCandidateStoreErrorKind::CorruptState,
        "local candidate receipt row is missing after the write",
    )
}

fn apply_missing_after_write() -> LocalCandidateStoreError {
    error(
        LocalCandidateStoreErrorKind::CorruptState,
        "local apply receipt row is missing after the write",
    )
}

fn cas_lost(action: &str) -> LocalCandidateStoreError {
    error(
        LocalCandidateStoreErrorKind::RevisionConflict,
        format!("local candidate compare-and-swap lost during {action}"),
    )
}

fn invalid(message: impl Into<String>) -> LocalCandidateStoreError {
    error(LocalCandidateStoreErrorKind::InvalidInput, message)
}

fn from_sql_integer(value: i64, label: &str) -> Result<u64, LocalCandidateStoreError> {
    let value = u64::try_from(value).map_err(|_| {
        error(
            LocalCandidateStoreErrorKind::CorruptState,
            format!("stored {label} is negative"),
        )
    })?;
    if value > MAX_SAFE_INTEGER {
        return Err(error(
            LocalCandidateStoreErrorKind::CorruptState,
            format!("stored {label} exceeds the safe integer range"),
        ));
    }
    Ok(value)
}

fn storage_error(storage: &StorageError) -> LocalCandidateStoreError {
    error(
        LocalCandidateStoreErrorKind::Storage,
        format!("local candidate ledger storage failed: {storage}"),
    )
}

fn sql_error(_sql: &rusqlite::Error) -> LocalCandidateStoreError {
    error(
        LocalCandidateStoreErrorKind::Storage,
        "local candidate ledger storage operation failed",
    )
}

fn error(
    kind: LocalCandidateStoreErrorKind,
    message: impl Into<String>,
) -> LocalCandidateStoreError {
    LocalCandidateStoreError {
        kind,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::{RepositoryAvailability, RepositoryBindingProjection, RepositoryDirtyState};

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(name: &str) -> PathBuf {
        let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        // Wall-clock nanos keep the directory unique even when the operating
        // system reuses a previous run's process id.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "winwincode-local-candidate-unit-{name}-{}-{suffix}-{nanos}",
            std::process::id()
        ))
    }

    fn unit_instant(value: &str) -> Instant {
        Instant(value.to_owned())
    }

    fn crockford(seed: u64) -> String {
        const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut identity = String::with_capacity(26);
        let mut value = seed;
        for _ in 0..26 {
            let digit = usize::try_from(value % 32).expect("digit fits");
            identity.push(ALPHABET[digit] as char);
            value /= 32;
        }
        identity
    }

    fn candidate_id(seed: u64) -> String {
        format!("lcr_{}", crockford(seed))
    }

    fn apply_id(seed: u64) -> String {
        format!("lar_{}", crockford(seed))
    }

    fn node_id(seed: u64) -> String {
        format!("cnd_{}", crockford(seed))
    }

    fn binding_id(seed: u64) -> String {
        format!("rbd_{}", crockford(seed))
    }

    fn candidate_ref(seed: u64) -> String {
        format!("refs/winwincode/candidates/candidate-{seed}")
    }

    const COMMIT: &str = "00112233445566778899aabbccddeeff00112233";
    const OTHER_COMMIT: &str = "ffeeddccbbaa99887766554433221100ffeeddcc";
    const RESULTING: &str = "1234567890abcdef1234567890abcdef12345678";

    fn seed_node(storage: &mut SqliteStorage, seed: u64) -> String {
        let client = node_id(seed);
        let registration = crate::ClientNodeRegistration::try_new(
            client.clone(),
            format!("{seed:010}"),
            format!("Device {seed}"),
            "aarch64-apple-darwin",
            "aarch64",
            "1.2.3",
            None,
            Some(format!("cix_{}", crockford(seed + 100))),
            2,
        )
        .expect("registration");
        let mut registry = storage.client_node_registry().expect("registry");
        registry
            .register(&registration, 0, &unit_instant("2026-01-01T00:00:00.000Z"))
            .expect("register");
        client
    }

    fn seed_binding(storage: &mut SqliteStorage, node: &str, seed: u64) -> String {
        let binding = binding_id(seed);
        let mut ledger = storage.repository_binding_ledger().expect("binding ledger");
        let projection = RepositoryBindingProjection::try_new(
            binding.clone(),
            node,
            "winwincode",
            Some("main".to_owned()),
            Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            RepositoryDirtyState::Clean,
            RepositoryAvailability::Available,
            format!("sha256:{seed:064}"),
        )
        .expect("projection");
        ledger
            .upsert(
                &projection,
                None,
                0,
                &unit_instant("2026-01-01T00:00:30.000Z"),
            )
            .expect("upsert");
        binding
    }

    fn retained_command(
        receipt_seed: u64,
        node: &str,
        binding: &str,
        ref_seed: u64,
        commit: &str,
    ) -> LocalCandidateRetained {
        LocalCandidateRetained::try_new(
            candidate_id(receipt_seed),
            node,
            binding,
            candidate_ref(ref_seed),
            commit,
            format!("refs/winwincode/candidates/candidate-{ref_seed}"),
        )
        .expect("retained command")
    }

    #[allow(clippy::too_many_arguments)]
    fn settlement_command(
        apply_seed: u64,
        receipt: &str,
        node: &str,
        binding: &str,
        ref_seed: u64,
        result: LocalApplyResult,
        resulting_commit: Option<String>,
        conflict_artifact_ref: Option<String>,
    ) -> LocalApplySettlement {
        try_settlement_command(
            apply_seed,
            receipt,
            node,
            binding,
            ref_seed,
            result,
            resulting_commit,
            conflict_artifact_ref,
        )
        .expect("settlement command")
    }

    #[allow(clippy::too_many_arguments)]
    fn try_settlement_command(
        apply_seed: u64,
        receipt: &str,
        node: &str,
        binding: &str,
        ref_seed: u64,
        result: LocalApplyResult,
        resulting_commit: Option<String>,
        conflict_artifact_ref: Option<String>,
    ) -> Result<LocalApplySettlement, LocalCandidateStoreError> {
        LocalApplySettlement::try_new(
            apply_id(apply_seed),
            receipt,
            node,
            binding,
            candidate_ref(ref_seed),
            "winwincode/main-branch".to_owned(),
            COMMIT.to_owned(),
            LocalApplyStrategy::FastForward,
            result,
            resulting_commit,
            conflict_artifact_ref,
        )
    }

    #[test]
    fn validates_retained_command_fields() {
        assert!(
            LocalCandidateRetained::try_new(
                candidate_id(1),
                node_id(2),
                binding_id(3),
                candidate_ref(4),
                COMMIT,
                "refs/winwincode/candidates/candidate-4"
            )
            .is_ok()
        );
        assert!(
            LocalCandidateRetained::try_new(
                "lcr_not-canonical",
                node_id(2),
                binding_id(3),
                candidate_ref(4),
                COMMIT,
                "refs/winwincode/candidates/candidate-4"
            )
            .is_err()
        );
        assert!(
            LocalCandidateRetained::try_new(
                candidate_id(1),
                node_id(2),
                binding_id(3),
                "refs/heads/main",
                COMMIT,
                "refs/winwincode/candidates/candidate-4"
            )
            .is_err()
        );
        assert!(
            LocalCandidateRetained::try_new(
                candidate_id(1),
                node_id(2),
                binding_id(3),
                candidate_ref(4),
                "NOT-A-COMMIT",
                "refs/winwincode/candidates/candidate-4"
            )
            .is_err()
        );
        assert!(
            LocalCandidateRetained::try_new(
                candidate_id(1),
                node_id(2),
                binding_id(3),
                candidate_ref(4),
                COMMIT,
                "-refs/leading-dash"
            )
            .is_err()
        );
    }

    #[test]
    fn validates_settlement_command_fields() {
        assert!(
            try_settlement_command(
                5,
                candidate_id(1).as_str(),
                node_id(2).as_str(),
                binding_id(3).as_str(),
                4,
                LocalApplyResult::Applied,
                Some(RESULTING.to_owned()),
                None,
            )
            .is_ok()
        );
        // An applied result without a resulting commit is rejected.
        assert!(
            try_settlement_command(
                5,
                candidate_id(1).as_str(),
                node_id(2).as_str(),
                binding_id(3).as_str(),
                4,
                LocalApplyResult::Applied,
                None,
                None,
            )
            .is_err()
        );
        // A failure result must not claim a resulting commit.
        assert!(
            try_settlement_command(
                5,
                candidate_id(1).as_str(),
                node_id(2).as_str(),
                binding_id(3).as_str(),
                4,
                LocalApplyResult::BaseStale,
                Some(RESULTING.to_owned()),
                None,
            )
            .is_err()
        );
        // Only a merge conflict may carry a conflict artifact reference.
        assert!(
            try_settlement_command(
                5,
                candidate_id(1).as_str(),
                node_id(2).as_str(),
                binding_id(3).as_str(),
                4,
                LocalApplyResult::BaseStale,
                None,
                Some("artifacts/conflict-1".to_owned()),
            )
            .is_err()
        );
        // A filesystem-looking artifact reference is rejected.
        assert!(
            try_settlement_command(
                5,
                candidate_id(1).as_str(),
                node_id(2).as_str(),
                binding_id(3).as_str(),
                4,
                LocalApplyResult::MergeConflict,
                None,
                Some("/tmp/conflict.patch".to_owned()),
            )
            .is_err()
        );
        assert!(
            try_settlement_command(
                5,
                candidate_id(1).as_str(),
                node_id(2).as_str(),
                binding_id(3).as_str(),
                4,
                LocalApplyResult::MergeConflict,
                None,
                Some("artifacts/conflict-1".to_owned()),
            )
            .is_ok()
        );
    }

    #[test]
    fn result_codes_project_onto_the_frozen_candidate_states() {
        assert_eq!(
            LocalApplyResult::Retained.candidate_state(),
            LocalCandidateReceiptState::Retained
        );
        assert_eq!(
            LocalApplyResult::BranchCreated.candidate_state(),
            LocalCandidateReceiptState::BranchCreated
        );
        assert_eq!(
            LocalApplyResult::Applied.candidate_state(),
            LocalCandidateReceiptState::Applied
        );
        assert_eq!(
            LocalApplyResult::Discarded.candidate_state(),
            LocalCandidateReceiptState::Discarded
        );
        for failure in [
            LocalApplyResult::BaseStale,
            LocalApplyResult::WorkingTreeDirty,
            LocalApplyResult::MergeConflict,
            LocalApplyResult::CandidateMissing,
            LocalApplyResult::PermissionDenied,
            LocalApplyResult::Failed,
        ] {
            assert!(failure.is_failure());
            assert_eq!(
                failure.candidate_state(),
                LocalCandidateReceiptState::Failed
            );
        }
        assert!(!LocalCandidateReceiptState::Failed.is_terminal());
        assert!(LocalCandidateReceiptState::Applied.is_terminal());
        assert!(LocalCandidateReceiptState::Discarded.is_terminal());
    }

    #[test]
    fn retained_append_is_idempotent_across_replays_and_duplicates() {
        let mut storage =
            SqliteStorage::open(temporary_directory("retained-idempotent")).expect("storage");
        let node = seed_node(&mut storage, 1);
        let binding = seed_binding(&mut storage, node.as_str(), 10);
        let command = retained_command(20, node.as_str(), binding.as_str(), 30, COMMIT);
        let first = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&command, &unit_instant("2026-01-01T01:00:00.000Z"))
                .expect("first retained")
        };
        assert_eq!(first.state, LocalCandidateReceiptState::Retained);
        assert_eq!(first.revision, 1);

        // Exact replay returns the original row unchanged.
        let replay = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&command, &unit_instant("2026-01-01T01:05:00.000Z"))
                .expect("replay")
        };
        assert_eq!(first, replay);
        assert_eq!(replay.revision, 1);

        // A duplicate report of the same candidate under a fresh receipt id
        // is also idempotent.
        let duplicate = retained_command(21, node.as_str(), binding.as_str(), 30, COMMIT);
        let merged = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&duplicate, &unit_instant("2026-01-01T01:06:00.000Z"))
                .expect("duplicate")
        };
        assert_eq!(first, merged);

        // Different facts for the same candidate ref fail closed.
        let conflicting = retained_command(22, node.as_str(), binding.as_str(), 30, OTHER_COMMIT);
        let mut ledger = storage.local_candidate_ledger().expect("ledger");
        let error = ledger
            .record_retained(&conflicting, &unit_instant("2026-01-01T01:07:00.000Z"))
            .expect_err("conflicting facts must be rejected");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::LocalCandidateConflict
        );
    }

    #[test]
    fn retained_names_unknown_owner_precisely() {
        let mut storage =
            SqliteStorage::open(temporary_directory("retained-unknown")).expect("storage");
        let node = seed_node(&mut storage, 2);
        let binding = seed_binding(&mut storage, node.as_str(), 11);
        let mut ledger = storage.local_candidate_ledger().expect("ledger");

        let unknown_binding =
            retained_command(23, node.as_str(), binding_id(99).as_str(), 31, COMMIT);
        let error = ledger
            .record_retained(&unknown_binding, &unit_instant("2026-01-01T01:00:00.000Z"))
            .expect_err("unknown binding must be rejected");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::UnknownRepositoryBinding
        );

        let unknown_node = retained_command(24, node_id(99).as_str(), binding.as_str(), 32, COMMIT);
        let error = ledger
            .record_retained(&unknown_node, &unit_instant("2026-01-01T01:00:01.000Z"))
            .expect_err("unknown client node must be rejected");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::UnknownClientNode
        );
    }

    #[test]
    fn apply_history_is_append_only_and_states_advance() {
        let mut storage =
            SqliteStorage::open(temporary_directory("append-only-history")).expect("storage");
        let node = seed_node(&mut storage, 3);
        let binding = seed_binding(&mut storage, node.as_str(), 12);
        let command = retained_command(25, node.as_str(), binding.as_str(), 33, COMMIT);
        {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&command, &unit_instant("2026-01-01T01:00:00.000Z"))
                .expect("retained");
        }
        let receipt = command.local_candidate_receipt_id().to_owned();

        // First attempt fails base-stale, then a branch is created, then the
        // candidate applies. Each attempt appends exactly one receipt.
        let attempts = [
            (
                40_u64,
                LocalApplyResult::BaseStale,
                None,
                "2026-01-01T01:01:00.000Z",
            ),
            (
                41,
                LocalApplyResult::BranchCreated,
                None,
                "2026-01-01T01:02:00.000Z",
            ),
            (
                42,
                LocalApplyResult::Applied,
                Some(RESULTING.to_owned()),
                "2026-01-01T01:03:00.000Z",
            ),
        ];
        for (seed, result, resulting, at) in attempts {
            let settlement = settlement_command(
                seed,
                receipt.as_str(),
                node.as_str(),
                binding.as_str(),
                33,
                result,
                resulting,
                None,
            );
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            let (apply_receipt, candidate) = ledger
                .record_apply_result(&settlement, &unit_instant(at))
                .expect("settlement");
            assert_eq!(apply_receipt.result, result);
            assert_eq!(apply_receipt.revision, 1);
            assert_eq!(candidate.state, result.candidate_state());
        }

        let ledger = storage.local_candidate_ledger().expect("ledger");
        let history = ledger
            .apply_history_for_candidate(receipt.as_str())
            .expect("history");
        assert_eq!(history.len(), 3);
        assert_eq!(
            history
                .iter()
                .map(|row| row.result.as_str())
                .collect::<Vec<_>>(),
            vec!["base_stale", "branch_created", "applied"]
        );
        let candidate = ledger
            .candidate_snapshot(receipt.as_str())
            .expect("snapshot")
            .expect("candidate");
        assert_eq!(candidate.state, LocalCandidateReceiptState::Applied);

        // The terminal candidate refuses any further settlement.
        let late = settlement_command(
            43,
            receipt.as_str(),
            node.as_str(),
            binding.as_str(),
            33,
            LocalApplyResult::Failed,
            None,
            None,
        );
        let mut ledger = storage.local_candidate_ledger().expect("ledger");
        let error = ledger
            .record_apply_result(&late, &unit_instant("2026-01-01T01:04:00.000Z"))
            .expect_err("terminal candidate must refuse further apply results");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::TerminalCandidateConflict
        );
    }

    #[test]
    fn apply_receipt_rows_are_immutable_at_the_database_layer() {
        let mut storage =
            SqliteStorage::open(temporary_directory("immutable-rows")).expect("storage");
        let node = seed_node(&mut storage, 7);
        let binding = seed_binding(&mut storage, node.as_str(), 17);
        let command = retained_command(29, node.as_str(), binding.as_str(), 38, COMMIT);
        let receipt = command.local_candidate_receipt_id().to_owned();
        {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&command, &unit_instant("2026-01-01T01:00:00.000Z"))
                .expect("retained");
            let settlement = settlement_command(
                51,
                receipt.as_str(),
                node.as_str(),
                binding.as_str(),
                38,
                LocalApplyResult::Applied,
                Some(RESULTING.to_owned()),
                None,
            );
            ledger
                .record_apply_result(&settlement, &unit_instant("2026-01-01T01:01:00.000Z"))
                .expect("settlement");
        }

        // The stored history rows are immutable: a stray UPDATE or DELETE at
        // the database layer is aborted by the durable triggers, so no later
        // settlement can ever rewrite an older attempt.
        for sql in [
            "UPDATE local_apply_receipts SET result = 'failed' WHERE local_apply_receipt_id = ?1",
            "DELETE FROM local_apply_receipts WHERE local_apply_receipt_id = ?1",
        ] {
            let error = storage
                .connection_mut()
                .expect("connection")
                .execute(sql, [apply_id(51)])
                .expect_err("apply receipt history must be immutable");
            assert!(
                error.to_string().contains("immutable"),
                "the immutable trigger must abort the write: {error}"
            );
        }
        let history = storage
            .local_candidate_ledger()
            .expect("ledger")
            .apply_history_for_candidate(receipt.as_str())
            .expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].result, LocalApplyResult::Applied);
    }

    #[test]
    fn failed_candidates_stay_retryable_and_settlements_replay_idempotently() {
        let mut storage =
            SqliteStorage::open(temporary_directory("failed-retryable")).expect("storage");
        let node = seed_node(&mut storage, 4);
        let binding = seed_binding(&mut storage, node.as_str(), 13);
        let command = retained_command(26, node.as_str(), binding.as_str(), 34, COMMIT);
        {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&command, &unit_instant("2026-01-01T01:00:00.000Z"))
                .expect("retained");
        }
        let receipt = command.local_candidate_receipt_id().to_owned();

        // A merge conflict keeps the candidate failed but records the
        // conflict artifact reference.
        let conflict = settlement_command(
            44,
            receipt.as_str(),
            node.as_str(),
            binding.as_str(),
            34,
            LocalApplyResult::MergeConflict,
            None,
            Some("artifacts/merge-conflict-1".to_owned()),
        );
        let (first, candidate) = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            let outcome = ledger
                .record_apply_result(&conflict, &unit_instant("2026-01-01T01:01:00.000Z"))
                .expect("conflict settlement");
            assert_eq!(
                outcome.0.conflict_artifact_ref.as_deref(),
                Some("artifacts/merge-conflict-1")
            );
            outcome
        };
        assert_eq!(candidate.state, LocalCandidateReceiptState::Failed);

        // Replaying the same settlement is an accepted idempotent no-op.
        let (replay, replay_candidate) = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_apply_result(&conflict, &unit_instant("2026-01-01T01:02:00.000Z"))
                .expect("replay")
        };
        assert_eq!(first, replay);
        assert_eq!(replay_candidate.state, LocalCandidateReceiptState::Failed);

        // The same receipt id with different fields fails closed.
        let mutated = settlement_command(
            44,
            receipt.as_str(),
            node.as_str(),
            binding.as_str(),
            34,
            LocalApplyResult::BaseStale,
            None,
            None,
        );
        let mut ledger = storage.local_candidate_ledger().expect("ledger");
        let error = ledger
            .record_apply_result(&mutated, &unit_instant("2026-01-01T01:03:00.000Z"))
            .expect_err("mutated settlement must be rejected");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::ApplyReceiptConflict
        );

        // Retry after failure succeeds: failed -> applied is legal.
        let retry = settlement_command(
            45,
            receipt.as_str(),
            node.as_str(),
            binding.as_str(),
            34,
            LocalApplyResult::Applied,
            Some(RESULTING.to_owned()),
            None,
        );
        let (_, applied) = ledger
            .record_apply_result(&retry, &unit_instant("2026-01-01T01:04:00.000Z"))
            .expect("retry");
        assert_eq!(applied.state, LocalCandidateReceiptState::Applied);
        let history = storage
            .local_candidate_ledger()
            .expect("ledger")
            .apply_history_for_candidate(receipt.as_str())
            .expect("history");
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn settlement_requires_the_exact_candidate_binding() {
        let mut storage =
            SqliteStorage::open(temporary_directory("precise-binding")).expect("storage");
        let node = seed_node(&mut storage, 5);
        let binding = seed_binding(&mut storage, node.as_str(), 14);
        let other_binding = seed_binding(&mut storage, node.as_str(), 15);
        let command = retained_command(27, node.as_str(), binding.as_str(), 35, COMMIT);
        {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&command, &unit_instant("2026-01-01T01:00:00.000Z"))
                .expect("retained");
        }
        let receipt = command.local_candidate_receipt_id().to_owned();
        let mut ledger = storage.local_candidate_ledger().expect("ledger");

        // Unknown candidate identity is reported precisely.
        let unknown = settlement_command(
            46,
            candidate_id(999).as_str(),
            node.as_str(),
            binding.as_str(),
            35,
            LocalApplyResult::Applied,
            Some(RESULTING.to_owned()),
            None,
        );
        let error = ledger
            .record_apply_result(&unknown, &unit_instant("2026-01-01T01:01:00.000Z"))
            .expect_err("unknown candidate must be rejected");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::UnknownLocalCandidate
        );

        // A settlement naming another binding does not match the candidate.
        let foreign = settlement_command(
            47,
            receipt.as_str(),
            node.as_str(),
            other_binding.as_str(),
            35,
            LocalApplyResult::Applied,
            Some(RESULTING.to_owned()),
            None,
        );
        let error = ledger
            .record_apply_result(&foreign, &unit_instant("2026-01-01T01:02:00.000Z"))
            .expect_err("foreign binding must be rejected");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::CandidateIdentityMismatch
        );

        // A settlement naming another candidate ref does not match either.
        let other_ref = settlement_command(
            48,
            receipt.as_str(),
            node.as_str(),
            binding.as_str(),
            36,
            LocalApplyResult::Applied,
            Some(RESULTING.to_owned()),
            None,
        );
        let error = ledger
            .record_apply_result(&other_ref, &unit_instant("2026-01-01T01:03:00.000Z"))
            .expect_err("foreign candidate ref must be rejected");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::CandidateIdentityMismatch
        );
    }

    #[test]
    fn retained_projection_rejects_non_retained_candidates_and_queries_stay_stable() {
        let mut storage =
            SqliteStorage::open(temporary_directory("retained-projection")).expect("storage");
        let node = seed_node(&mut storage, 6);
        let binding = seed_binding(&mut storage, node.as_str(), 16);
        let command = retained_command(28, node.as_str(), binding.as_str(), 37, COMMIT);
        {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&command, &unit_instant("2026-01-01T01:00:00.000Z"))
                .expect("retained");
        }
        let receipt = command.local_candidate_receipt_id().to_owned();

        // branch_created moves the candidate out of plain `retained`.
        let branch = settlement_command(
            49,
            receipt.as_str(),
            node.as_str(),
            binding.as_str(),
            37,
            LocalApplyResult::BranchCreated,
            None,
            None,
        );
        let (_, candidate) = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_apply_result(&branch, &unit_instant("2026-01-01T01:01:00.000Z"))
                .expect("branch settlement")
        };
        assert_eq!(candidate.state, LocalCandidateReceiptState::BranchCreated);

        // A `retained` outcome is only a projection of the plain `retained`
        // state and is not legal from `branch_created`.
        let retained_outcome = settlement_command(
            50,
            receipt.as_str(),
            node.as_str(),
            binding.as_str(),
            37,
            LocalApplyResult::Retained,
            None,
            None,
        );
        let mut ledger = storage.local_candidate_ledger().expect("ledger");
        let error = ledger
            .record_apply_result(&retained_outcome, &unit_instant("2026-01-01T01:02:00.000Z"))
            .expect_err("retained outcome from branch_created is not legal");
        assert_eq!(
            error.kind(),
            LocalCandidateStoreErrorKind::IllegalStateTransition
        );

        // Candidate lookups agree across both identities.
        let ledger = storage.local_candidate_ledger().expect("ledger");
        let by_ref = ledger
            .candidate_for_ref(node.as_str(), candidate_ref(37).as_str())
            .expect("by ref")
            .expect("candidate by ref");
        assert_eq!(candidate, by_ref);
        assert!(
            ledger
                .candidate_for_ref(node.as_str(), candidate_ref(38).as_str())
                .expect("missing ref")
                .is_none()
        );
        assert!(
            ledger
                .apply_receipt(apply_id(999).as_str())
                .expect("apply")
                .is_none()
        );
        let apply = ledger
            .apply_receipt(apply_id(49).as_str())
            .expect("apply")
            .expect("apply by id");
        assert_eq!(apply.result, LocalApplyResult::BranchCreated);
    }
}
