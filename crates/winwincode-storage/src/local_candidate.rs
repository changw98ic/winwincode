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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCandidateReceiptState {
    Retained,
    BranchCreated,
    Applied,
    Discarded,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalApplyStrategy {
    CreateBranch,
    FastForward,
    CherryPick,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalApplyResult {
    Retained,
    BranchCreated,
    Applied,
    BaseStale,
    WorkingTreeDirty,
    MergeConflict,
    CandidateMissing,
    PermissionDenied,
    Discarded,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCandidateReceiptRecord {
    pub local_candidate_receipt_id: String,
    pub client_node_id: String,
    pub repository_binding_id: String,
    pub candidate_ref: String,
    pub candidate_commit: String,
    pub local_ref_name: String,
    pub state: LocalCandidateReceiptState,
    pub created_at: Instant,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalApplyReceiptRecord {
    pub local_apply_receipt_id: String,
    pub local_candidate_receipt_id: String,
    pub client_node_id: String,
    pub repository_binding_id: String,
    pub candidate_ref: String,
    pub target_branch: String,
    pub expected_head: String,
    pub strategy: LocalApplyStrategy,
    pub result: LocalApplyResult,
    pub resulting_commit: Option<String>,
    pub conflict_artifact_ref: Option<String>,
    pub created_at: Instant,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCandidateStoreErrorKind {
    InvalidInput,
    UnknownClientNode,
    UnknownRepositoryBinding,
    UnknownLocalCandidate,
    UnknownLocalApplyReceipt,
    LocalCandidateConflict,
    ApplyReceiptConflict,
    TerminalCandidateConflict,
    CandidateIdentityMismatch,
    IllegalStateTransition,
    RevisionConflict,
    CorruptState,
    Storage,
}

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

pub struct LocalCandidateLedger<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
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

    pub fn candidate_snapshot(
        &self,
        local_candidate_receipt_id: &str,
    ) -> Result<Option<LocalCandidateReceiptRecord>, LocalCandidateStoreError> {
        validate_candidate_receipt_id(local_candidate_receipt_id)?;
        load_candidate(self.connection()?, local_candidate_receipt_id)
    }

    pub fn candidate_for_ref(
        &self,
        client_node_id: &str,
        candidate_ref: &str,
    ) -> Result<Option<LocalCandidateReceiptRecord>, LocalCandidateStoreError> {
        validate_client_node_id(client_node_id)?;
        validate_candidate_ref(candidate_ref)?;
        load_candidate_by_ref(self.connection()?, client_node_id, candidate_ref)
    }

    pub fn apply_receipt(
        &self,
        local_apply_receipt_id: &str,
    ) -> Result<Option<LocalApplyReceiptRecord>, LocalCandidateStoreError> {
        validate_apply_receipt_id(local_apply_receipt_id)?;
        load_apply(self.connection()?, local_apply_receipt_id)
    }

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

fn ensure_legal_projection(
    candidate: &LocalCandidateReceiptRecord,
    result: LocalApplyResult,
) -> Result<(), LocalCandidateStoreError> {
    let legal = match result {
        LocalApplyResult::Retained => candidate.state == LocalCandidateReceiptState::Retained,
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

        let replay = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&command, &unit_instant("2026-01-01T01:05:00.000Z"))
                .expect("replay")
        };
        assert_eq!(first, replay);
        assert_eq!(replay.revision, 1);

        let duplicate = retained_command(21, node.as_str(), binding.as_str(), 30, COMMIT);
        let merged = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_retained(&duplicate, &unit_instant("2026-01-01T01:06:00.000Z"))
                .expect("duplicate")
        };
        assert_eq!(first, merged);

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

        let (replay, replay_candidate) = {
            let mut ledger = storage.local_candidate_ledger().expect("ledger");
            ledger
                .record_apply_result(&conflict, &unit_instant("2026-01-01T01:02:00.000Z"))
                .expect("replay")
        };
        assert_eq!(first, replay);
        assert_eq!(replay_candidate.state, LocalCandidateReceiptState::Failed);

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
