// SPDX-License-Identifier: Apache-2.0

//! Durable execution-resource admission and budget reservations.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Sha256Digest, UserId, WorkspaceId,
};

use crate::{ExecutionQueueScope, SqliteStorage, StorageError};

const ADMISSION_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS execution_admission_policies (
    boundary_key TEXT PRIMARY KEY NOT NULL,
    boundary_json TEXT NOT NULL,
    max_concurrent INTEGER NOT NULL CHECK (max_concurrent >= 0),
    max_queued INTEGER NOT NULL CHECK (max_queued >= 0),
    token_budget INTEGER CHECK (token_budget >= 0),
    cost_budget_microunits INTEGER CHECK (cost_budget_microunits >= 0),
    max_runtime_millis INTEGER CHECK (max_runtime_millis > 0)
);
CREATE TABLE IF NOT EXISTS execution_attempt_accounting_pending (
 job_id TEXT NOT NULL,attempt INTEGER NOT NULL,lease_json TEXT NOT NULL,
 observed_at TEXT NOT NULL,PRIMARY KEY(job_id,attempt)
);
CREATE TABLE IF NOT EXISTS execution_admission_reservations (
    job_id TEXT PRIMARY KEY NOT NULL,
    scope_key TEXT NOT NULL,
    organization_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    delivery_id TEXT,
    product_session_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    worker_pool_id TEXT NOT NULL,
    access_kind TEXT NOT NULL CHECK (
        access_kind IN ('read_only', 'shared_write', 'isolated_write')
    ),
    worktree_key TEXT,
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'released', 'settled')),
    reserved_tokens INTEGER CHECK (reserved_tokens >= 0),
    reserved_cost_microunits INTEGER CHECK (reserved_cost_microunits >= 0),
    runtime_limit_millis INTEGER CHECK (runtime_limit_millis > 0),
    actual_tokens INTEGER,
    actual_cost_microunits INTEGER,
    actual_runtime_millis INTEGER,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    submitted_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    started_at TEXT,
    terminal_at TEXT,
    release_reason TEXT CHECK (release_reason IN ('cancelled', 'failed')),
    CHECK (
        (access_kind = 'isolated_write' AND worktree_key IS NOT NULL)
        OR (access_kind != 'isolated_write' AND worktree_key IS NULL)
    ),
    CHECK (
        (state = 'settled' AND actual_tokens IS NOT NULL
            AND actual_runtime_millis IS NOT NULL
            AND terminal_at IS NOT NULL AND release_reason IS NULL)
        OR (state = 'released' AND actual_tokens IS NULL
            AND actual_cost_microunits IS NULL AND actual_runtime_millis IS NULL
            AND terminal_at IS NOT NULL AND release_reason IS NOT NULL)
        OR (state IN ('queued', 'running') AND actual_tokens IS NULL
            AND actual_cost_microunits IS NULL AND actual_runtime_millis IS NULL
            AND terminal_at IS NULL AND release_reason IS NULL)
    )
);
CREATE TABLE IF NOT EXISTS execution_admission_reservation_boundaries (
    job_id TEXT NOT NULL,
    boundary_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    PRIMARY KEY (job_id, boundary_key),
    UNIQUE (job_id, ordinal),
    FOREIGN KEY (job_id) REFERENCES execution_admission_reservations(job_id) ON DELETE CASCADE,
    FOREIGN KEY (boundary_key) REFERENCES execution_admission_policies(boundary_key)
);
CREATE TABLE IF NOT EXISTS execution_admission_receipts (
    scope_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (scope_key, request_id)
);
CREATE TABLE IF NOT EXISTS execution_admission_settlement_sources (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT CHECK (sequence > 0),
    source_key TEXT UNIQUE NOT NULL,
    source_digest TEXT NOT NULL,
    fact_json TEXT NOT NULL,
    job_id TEXT UNIQUE NOT NULL,
    settlement_request_id TEXT NOT NULL,
    FOREIGN KEY (job_id) REFERENCES execution_admission_reservations(job_id) ON DELETE RESTRICT
);
CREATE TABLE IF NOT EXISTS execution_admission_reconciliation (
    job_id TEXT PRIMARY KEY NOT NULL REFERENCES execution_admission_reservations(job_id),
    terminal_request_id TEXT NOT NULL, terminal_usage_json TEXT NOT NULL,
    accounting_status TEXT NOT NULL CHECK(accounting_status IN ('pending','settled')),
    settlement_request_id TEXT, settlement_usage_json TEXT, observed_at TEXT,
    CHECK((accounting_status='pending' AND settlement_request_id IS NULL AND settlement_usage_json IS NULL)
      OR (accounting_status='settled' AND settlement_request_id IS NOT NULL AND settlement_usage_json IS NOT NULL))
);
CREATE TABLE IF NOT EXISTS execution_admission_reconciliation_facts (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id TEXT NOT NULL REFERENCES execution_admission_reconciliation(job_id),
    settlement_request_id TEXT UNIQUE NOT NULL, usage_json TEXT NOT NULL, observed_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS execution_admission_boundary_usage
    ON execution_admission_reservation_boundaries (boundary_key, job_id);
CREATE INDEX IF NOT EXISTS execution_admission_repository_writes
    ON execution_admission_reservations (
        organization_id, project_id, repository_id, state, access_kind, worktree_key
    );
CREATE INDEX IF NOT EXISTS execution_admission_settlement_receipts
    ON execution_admission_settlement_sources (settlement_request_id, sequence);
";

const RECONCILIATION_BACKFILL: &str = r"
INSERT OR IGNORE INTO execution_admission_reconciliation (job_id,terminal_request_id,terminal_usage_json,accounting_status)
SELECT r.job_id,s.settlement_request_id,json_object('runtimeMillis',r.actual_runtime_millis,'tokens',r.actual_tokens,'knownTokens',r.actual_tokens,'accountingStatus','known','costMicrounits',NULL),'pending'
FROM execution_admission_reservations r JOIN execution_admission_settlement_sources s ON s.job_id=r.job_id
WHERE r.state='settled' AND r.actual_cost_microunits IS NULL AND r.actual_tokens IS NOT NULL AND r.actual_runtime_millis IS NOT NULL;
";

const MAX_SETTLEMENT_SOURCE_PAGE_SIZE: u64 = 200;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn prepare_admission_schema(connection: &mut Connection) -> Result<(), ExecutionAdmissionError> {
    let needs_migration: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('execution_admission_policies') WHERE name IN ('max_runtime_millis', 'token_budget', 'cost_budget_microunits') AND \"notnull\" = 1) OR EXISTS(SELECT 1 FROM pragma_table_info('execution_admission_reservations') WHERE name IN ('runtime_limit_millis', 'reserved_tokens', 'reserved_cost_microunits') AND \"notnull\" = 1)",
        [], |row| row.get(0),
    ).map_err(sql_error)?;
    let requires_known_cost: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'execution_admission_reservations' AND instr(sql, 'actual_cost_microunits IS NOT NULL') > 0)",
        [], |row| row.get(0),
    ).map_err(sql_error)?;
    if !needs_migration && !requires_known_cost {
        return connection
            .execute_batch(ADMISSION_SCHEMA)
            .map_err(sql_error);
    }
    // Rebuild nullable limits in place. Foreign keys are checked before
    // committing and restored even when the migration rolls back.
    connection
        .pragma_update(None, "foreign_keys", false)
        .map_err(sql_error)?;
    let result = (|| {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        for (table, columns) in [
            (
                "execution_admission_policies",
                [
                    "max_runtime_millis",
                    "token_budget",
                    "cost_budget_microunits",
                ],
            ),
            (
                "execution_admission_reservations",
                [
                    "runtime_limit_millis",
                    "reserved_tokens",
                    "reserved_cost_microunits",
                ],
            ),
        ] {
            let required: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name IN (?2, ?3, ?4) AND \"notnull\" = 1)",
                params![table, columns[0], columns[1], columns[2]], |row| row.get(0),
            ).map_err(sql_error)?;
            if !(required || table == "execution_admission_reservations" && requires_known_cost) {
                continue;
            }
            let prefix = format!("CREATE TABLE IF NOT EXISTS {table} (");
            let start = ADMISSION_SCHEMA.find(&prefix).ok_or_else(|| {
                ExecutionAdmissionError::adapter("missing admission table definition")
            })?;
            let definition = ADMISSION_SCHEMA[start..]
                .split_once(';')
                .ok_or_else(|| {
                    ExecutionAdmissionError::adapter("invalid admission table definition")
                })?
                .0;
            let replacement = format!("{table}_limits_migration");
            transaction.execute_batch(&format!(
                "{}; INSERT INTO {replacement} SELECT * FROM {table}; DROP TABLE {table}; ALTER TABLE {replacement} RENAME TO {table};",
                definition.replacen(&prefix, &format!("CREATE TABLE {replacement} ("), 1),
            )).map_err(sql_error)?;
        }
        transaction
            .execute_batch(ADMISSION_SCHEMA)
            .map_err(sql_error)?;
        let invalid: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if invalid {
            return Err(ExecutionAdmissionError::adapter(
                "admission migration violates foreign keys",
            ));
        }
        transaction.commit().map_err(sql_error)
    })();
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(sql_error)?;
    result
}

/// Canonical worker-pool identity used by resource admission.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct WorkerPoolId(pub String);

/// One independently limited scheduler boundary.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExecutionAdmissionBoundary {
    Organization {
        organization_id: OrganizationId,
    },
    Project {
        organization_id: OrganizationId,
        project_id: ProjectId,
    },
    Repository {
        organization_id: OrganizationId,
        project_id: ProjectId,
        repository_id: RepositoryId,
    },
    Delivery {
        organization_id: OrganizationId,
        delivery_id: DeliveryId,
    },
    ProductSession {
        organization_id: OrganizationId,
        project_id: ProjectId,
        product_session_id: ProductSessionId,
    },
    WorkerPool {
        organization_id: OrganizationId,
        worker_pool_id: WorkerPoolId,
    },
}

/// Fixed limits for one scheduler boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionAdmissionLimits {
    pub max_concurrent: u64,
    pub max_queued: u64,
    /// `None` explicitly permits unlimited usage; actual usage is still recorded.
    #[serde(deserialize_with = "Option::deserialize")]
    pub token_budget: Option<u64>,
    /// `None` explicitly permits unlimited usage; actual usage is still recorded.
    #[serde(deserialize_with = "Option::deserialize")]
    pub cost_budget_microunits: Option<u64>,
    /// `None` explicitly permits execution without a runtime limit.
    #[serde(deserialize_with = "Option::deserialize")]
    pub max_runtime_millis: Option<u64>,
}

/// Immutable configured policy for one scheduler boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionAdmissionPolicy {
    pub boundary: ExecutionAdmissionBoundary,
    pub limits: ExecutionAdmissionLimits,
}

/// Repository access determines whether two writes may run together.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExecutionRepositoryAccess {
    ReadOnly,
    SharedWrite,
    IsolatedWrite { worktree_key: String },
}

/// Durable admission reservation lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionReservationState {
    Queued,
    Running,
    Released,
    Settled,
}

impl ExecutionReservationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Released => "released",
            Self::Settled => "settled",
        }
    }

    fn parse(value: &str) -> Result<Self, ExecutionAdmissionError> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "released" => Ok(Self::Released),
            "settled" => Ok(Self::Settled),
            _ => Err(ExecutionAdmissionError::adapter(
                "stored execution reservation state is invalid",
            )),
        }
    }
}

/// Why a queued/running reservation stopped without a completed result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionReservationReleaseReason {
    Cancelled,
    Failed,
}

impl ExecutionReservationReleaseReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, ExecutionAdmissionError> {
        match value {
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            _ => Err(ExecutionAdmissionError::adapter(
                "stored execution reservation release reason is invalid",
            )),
        }
    }
}

/// Queue admission and full budget reservation request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionReservationRequest {
    pub scope: ExecutionQueueScope,
    pub user_id: UserId,
    pub worker_pool_id: WorkerPoolId,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub repository_access: ExecutionRepositoryAccess,
    /// `None` explicitly permits unlimited usage; actual usage is still recorded.
    #[serde(deserialize_with = "Option::deserialize")]
    pub reserved_tokens: Option<u64>,
    /// `None` explicitly permits unlimited usage; actual usage is still recorded.
    #[serde(deserialize_with = "Option::deserialize")]
    pub reserved_cost_microunits: Option<u64>,
    /// `None` explicitly permits execution without a runtime limit.
    #[serde(deserialize_with = "Option::deserialize")]
    pub runtime_limit_millis: Option<u64>,
    pub submitted_at: Instant,
}

/// Starts one queued reservation after concurrent-resource checks.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionReservationStart {
    pub scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub started_at: Instant,
}

/// Releases all queue, concurrency, and unspent budget reservations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionReservationRelease {
    pub scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub reason: ExecutionReservationReleaseReason,
    pub released_at: Instant,
}

/// Settles a completed execution against actual usage.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionReservationSettlement {
    pub scope: ExecutionQueueScope,
    pub worker_pool_id: WorkerPoolId,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub actual_tokens: u64,
    /// None records an unknown Provider charge, never a free call.
    #[serde(deserialize_with = "Option::deserialize")]
    pub actual_cost_microunits: Option<u64>,
    pub actual_runtime_millis: u64,
    pub completed_at: Instant,
}

/// Complete durable reservation record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionReservationRecord {
    pub scope: ExecutionQueueScope,
    pub user_id: UserId,
    pub worker_pool_id: WorkerPoolId,
    pub job_id: ExecutionJobId,
    pub repository_access: ExecutionRepositoryAccess,
    pub state: ExecutionReservationState,
    /// `None` explicitly permits unlimited usage; actual usage is still recorded.
    #[serde(deserialize_with = "Option::deserialize")]
    pub reserved_tokens: Option<u64>,
    /// `None` explicitly permits unlimited usage; actual usage is still recorded.
    #[serde(deserialize_with = "Option::deserialize")]
    pub reserved_cost_microunits: Option<u64>,
    /// `None` explicitly permits execution without a runtime limit.
    #[serde(deserialize_with = "Option::deserialize")]
    pub runtime_limit_millis: Option<u64>,
    pub actual_tokens: Option<u64>,
    pub actual_cost_microunits: Option<u64>,
    pub actual_runtime_millis: Option<u64>,
    pub revision: u64,
    pub submitted_at: Instant,
    pub updated_at: Instant,
    pub started_at: Option<Instant>,
    pub terminal_at: Option<Instant>,
    pub release_reason: Option<ExecutionReservationReleaseReason>,
}

/// Immutable worker usage fact committed with a successful settlement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerSettlementSourceFact {
    pub job_id: ExecutionJobId,
    pub settlement_request_id: RequestId,
    pub worker_pool_id: WorkerPoolId,
    pub scope: ExecutionQueueScope,
    pub user_id: UserId,
    pub actual_runtime_millis: u64,
    pub actual_tokens: u64,
    /// None records an unknown Provider charge, never a free call.
    #[serde(deserialize_with = "Option::deserialize")]
    pub actual_cost_microunits: Option<u64>,
    pub completed_at: Instant,
}

/// One immutable worker-settlement source for usage reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSettlementSourceEntry {
    pub sequence: u64,
    pub source_digest: Sha256Digest,
    pub fact: WorkerSettlementSourceFact,
}

/// Stable cursor over one immutable worker-settlement source snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSettlementSourceCursor {
    snapshot_sequence: u64,
    after_sequence: u64,
}

impl WorkerSettlementSourceCursor {
    #[must_use]
    pub const fn snapshot_sequence(&self) -> u64 {
        self.snapshot_sequence
    }

    #[must_use]
    pub const fn after_sequence(&self) -> u64 {
        self.after_sequence
    }
}

/// One bounded page from a fixed worker-settlement source snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerSettlementSourcePage {
    pub snapshot_sequence: u64,
    pub entries: Vec<WorkerSettlementSourceEntry>,
    pub next: Option<WorkerSettlementSourceCursor>,
}

/// Replay-safe response for reserve/start/release/settle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionAdmissionReceipt {
    pub reservation: ExecutionReservationRecord,
    pub replayed: bool,
}

/// Transactionally consistent usage for one configured boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionAdmissionUsage {
    pub queued: u64,
    pub running: u64,
    pub reserved_tokens: u64,
    pub reserved_cost_microunits: u64,
    pub committed_tokens: u64,
    /// Finished executions awaiting a final token total; execution slots are already free.
    pub accounting_pending: u64,
    /// None means at least one settled charge is unknown.
    pub committed_cost_microunits: Option<u64>,
}

impl Default for ExecutionAdmissionUsage {
    fn default() -> Self {
        Self {
            queued: 0,
            running: 0,
            reserved_tokens: 0,
            reserved_cost_microunits: 0,
            committed_tokens: 0,
            accounting_pending: 0,
            committed_cost_microunits: Some(0),
        }
    }
}

/// Stable machine-readable rejection and adapter failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionAdmissionErrorCode {
    InvalidInput,
    RequestConflict,
    RevisionConflict,
    PolicyMissing,
    QueueCapacityExhausted,
    ConcurrencyExhausted,
    TokenBudgetExhausted,
    CostBudgetExhausted,
    RuntimeLimitExceeded,
    RepositoryWriteConflict,
    Adapter,
}

/// Admission error with a stable code and optional failing boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionAdmissionError {
    code: ExecutionAdmissionErrorCode,
    boundary: Option<ExecutionAdmissionBoundary>,
    message: String,
}

impl ExecutionAdmissionError {
    fn new(
        code: ExecutionAdmissionErrorCode,
        boundary: Option<ExecutionAdmissionBoundary>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            boundary,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(ExecutionAdmissionErrorCode::InvalidInput, None, message)
    }

    fn adapter(message: impl Into<String>) -> Self {
        Self::new(ExecutionAdmissionErrorCode::Adapter, None, message)
    }

    fn rejected(
        code: ExecutionAdmissionErrorCode,
        boundary: &ExecutionAdmissionBoundary,
        message: &'static str,
    ) -> Self {
        Self::new(code, Some(boundary.clone()), message)
    }

    #[must_use]
    pub const fn code(&self) -> ExecutionAdmissionErrorCode {
        self.code
    }

    #[must_use]
    pub const fn boundary(&self) -> Option<&ExecutionAdmissionBoundary> {
        self.boundary.as_ref()
    }
}

impl fmt::Display for ExecutionAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExecutionAdmissionError {}

/// SQLite-backed execution admission service.
pub struct ExecutionAdmission<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens durable execution admission over this storage connection.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the admission schema cannot be prepared.
    pub fn execution_admission(
        &mut self,
    ) -> Result<ExecutionAdmission<'_>, ExecutionAdmissionError> {
        ExecutionAdmission::new(self)
    }
}

impl<'storage> ExecutionAdmission<'storage> {
    fn new(storage: &'storage mut SqliteStorage) -> Result<Self, ExecutionAdmissionError> {
        let admission = Self { storage };
        prepare_admission_schema(admission.storage.connection_mut().map_err(storage_error)?)?;
        admission
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .execute_batch(RECONCILIATION_BACKFILL)
            .map_err(sql_error)?;
        Ok(admission)
    }

    /// Installs one immutable boundary policy. Exact repeats are idempotent;
    /// changing an installed policy is rejected so concurrent callers cannot
    /// weaken an active limit.
    ///
    /// # Errors
    ///
    /// Rejects malformed boundaries/limits, changed policies, and `SQLite` failures.
    pub fn configure_policy(
        &mut self,
        policy: &ExecutionAdmissionPolicy,
    ) -> Result<bool, ExecutionAdmissionError> {
        validate_policy(policy)?;
        let boundary_key = boundary_key(&policy.boundary)?;
        let boundary_json = encode_json(&policy.boundary)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let existing = transaction
            .query_row(
                "SELECT boundary_json, max_concurrent, max_queued, token_budget,
                        cost_budget_microunits, max_runtime_millis
                 FROM execution_admission_policies WHERE boundary_key = ?1",
                [&boundary_key],
                stored_policy_row,
            )
            .optional()
            .map_err(sql_error)?;
        if let Some(existing) = existing {
            let existing = complete_policy(&existing)?;
            if existing != *policy {
                return Err(ExecutionAdmissionError::invalid(
                    "execution admission policy is already configured differently",
                ));
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(false);
        }

        transaction
            .execute(
                "INSERT INTO execution_admission_policies
                    (boundary_key, boundary_json, max_concurrent, max_queued, token_budget,
                     cost_budget_microunits, max_runtime_millis)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    boundary_key,
                    boundary_json,
                    to_sql_integer(policy.limits.max_concurrent, "maxConcurrent")?,
                    to_sql_integer(policy.limits.max_queued, "maxQueued")?,
                    policy
                        .limits
                        .token_budget
                        .map(|value| to_sql_integer(value, "tokenBudget"))
                        .transpose()?,
                    policy
                        .limits
                        .cost_budget_microunits
                        .map(|value| to_sql_integer(value, "costBudgetMicrounits"))
                        .transpose()?,
                    policy
                        .limits
                        .max_runtime_millis
                        .map(|value| to_sql_integer(value, "maxRuntimeMillis"))
                        .transpose()?,
                ],
            )
            .map_err(sql_error)?;
        transaction.commit().map_err(sql_error)?;
        Ok(true)
    }

    /// Atomically reserves queue capacity plus the requested token and cost
    /// budgets across every boundary in the request.
    ///
    /// # Errors
    ///
    /// Rejects invalid input, missing/exhausted policies, conflicting replays,
    /// duplicate jobs, and `SQLite` failures.
    pub fn reserve(
        &mut self,
        request: &ExecutionReservationRequest,
    ) -> Result<ExecutionAdmissionReceipt, ExecutionAdmissionError> {
        validate_reservation_request(request)?;
        let scope_key = reservation_scope_key(&request.scope, &request.worker_pool_id)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        if reservation_exists(&transaction, &request.job_id)? {
            return Err(ExecutionAdmissionError::invalid(
                "execution reservation job identity already exists",
            ));
        }

        let boundaries = request_boundaries(&request.scope, &request.worker_pool_id);
        ensure_reservation_capacity(&transaction, request, &boundaries)?;
        insert_reservation(&transaction, &scope_key, request, &boundaries)?;

        let reservation = load_scoped_reservation(
            &transaction,
            &request.scope,
            &request.worker_pool_id,
            &request.job_id,
        )?
        .ok_or_else(|| ExecutionAdmissionError::adapter("reserved execution job was not stored"))?;
        let receipt = ExecutionAdmissionReceipt {
            reservation,
            replayed: false,
        };
        insert_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
            &receipt,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    /// Atomically consumes concurrency slots and starts one queued reservation.
    ///
    /// # Errors
    ///
    /// Rejects stale/invalid requests, exhausted concurrency, repository-write
    /// conflicts, conflicting replays, and `SQLite` failures.
    pub fn start(
        &mut self,
        request: &ExecutionReservationStart,
    ) -> Result<ExecutionAdmissionReceipt, ExecutionAdmissionError> {
        validate_start(request)?;
        let scope_key = reservation_scope_key(&request.scope, &request.worker_pool_id)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let current = load_scoped_reservation(
            &transaction,
            &request.scope,
            &request.worker_pool_id,
            &request.job_id,
        )?
        .ok_or_else(|| ExecutionAdmissionError::invalid("execution reservation does not exist"))?;
        validate_revision_state_time(
            &current,
            request.expected_revision,
            ExecutionReservationState::Queued,
            &request.started_at,
        )?;

        for boundary in request_boundaries(&request.scope, &request.worker_pool_id) {
            let (key, policy) = required_policy(&transaction, &boundary)?;
            if load_usage(&transaction, &key)?.running >= policy.limits.max_concurrent {
                return Err(ExecutionAdmissionError::rejected(
                    ExecutionAdmissionErrorCode::ConcurrencyExhausted,
                    &boundary,
                    "execution concurrency is exhausted",
                ));
            }
        }
        if has_repository_write_conflict(&transaction, &current)? {
            let boundary = ExecutionAdmissionBoundary::Repository {
                organization_id: current.scope.organization_id.clone(),
                project_id: current.scope.project_id.clone(),
                repository_id: current.scope.repository_id.clone(),
            };
            return Err(ExecutionAdmissionError::rejected(
                ExecutionAdmissionErrorCode::RepositoryWriteConflict,
                &boundary,
                "repository write requires serialization or a distinct worktree",
            ));
        }

        transition_reservation(
            &transaction,
            &request.job_id,
            request.expected_revision,
            ExecutionReservationState::Queued,
            ExecutionReservationState::Running,
            &request.started_at,
            Some(&request.started_at),
            None,
            None,
        )?;
        finish_mutation(
            &transaction,
            &scope_key,
            &request.scope,
            &request.worker_pool_id,
            &request.job_id,
            &request.request_id,
            &request_digest,
        )
        .and_then(|receipt| {
            transaction.commit().map_err(sql_error)?;
            Ok(receipt)
        })
    }

    /// Releases a queued or running reservation after cancellation/failure.
    ///
    /// # Errors
    ///
    /// Rejects stale/terminal requests, conflicting replays, and `SQLite` failures.
    pub fn release(
        &mut self,
        request: &ExecutionReservationRelease,
    ) -> Result<ExecutionAdmissionReceipt, ExecutionAdmissionError> {
        self.release_with_accounting(request, None)
    }

    /// Ends execution while retaining unknown usage for durable budget reconciliation.
    /// # Errors
    /// Rejects inconsistent accounting, changed terminal replay, or storage failure.
    pub fn release_with_unknown_usage(
        &mut self,
        request: &ExecutionReservationRelease,
        usage: &winwincode_execution_port::generated::ExecutionOutcomeUsage,
    ) -> Result<ExecutionAdmissionReceipt, ExecutionAdmissionError> {
        if !winwincode_execution_port::usage::valid_usage(usage) || usage.tokens.is_some() {
            return Err(ExecutionAdmissionError::invalid(
                "unknown terminal usage is inconsistent",
            ));
        }
        self.release_with_accounting(request, Some(usage))
    }

    fn release_with_accounting(
        &mut self,
        request: &ExecutionReservationRelease,
        usage: Option<&winwincode_execution_port::generated::ExecutionOutcomeUsage>,
    ) -> Result<ExecutionAdmissionReceipt, ExecutionAdmissionError> {
        validate_release(request)?;
        let scope_key = reservation_scope_key(&request.scope, &request.worker_pool_id)?;
        let request_digest = if let Some(usage) = usage {
            digest(&(request, usage))?
        } else {
            digest(request)?
        };
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let current = load_scoped_reservation(
            &transaction,
            &request.scope,
            &request.worker_pool_id,
            &request.job_id,
        )?
        .ok_or_else(|| ExecutionAdmissionError::invalid("execution reservation does not exist"))?;
        if !matches!(
            current.state,
            ExecutionReservationState::Queued | ExecutionReservationState::Running
        ) {
            return Err(ExecutionAdmissionError::invalid(
                "terminal execution reservation cannot be released",
            ));
        }
        validate_revision_time(&current, request.expected_revision, &request.released_at)?;
        transition_reservation(
            &transaction,
            &request.job_id,
            request.expected_revision,
            current.state,
            ExecutionReservationState::Released,
            &request.released_at,
            current.started_at.as_ref(),
            Some(&request.released_at),
            Some(request.reason),
        )?;
        if let Some(usage) = usage {
            let usage_json = serde_json::to_string(usage).map_err(|_| {
                ExecutionAdmissionError::invalid("terminal accounting cannot be encoded")
            })?;
            transaction.execute("INSERT INTO execution_admission_reconciliation (job_id,terminal_request_id,terminal_usage_json,accounting_status) VALUES (?1,?2,?3,'pending')",
                params![request.job_id.0,request.request_id.0,usage_json]).map_err(sql_error)?;
        }
        finish_mutation(
            &transaction,
            &scope_key,
            &request.scope,
            &request.worker_pool_id,
            &request.job_id,
            &request.request_id,
            &request_digest,
        )
        .and_then(|receipt| {
            transaction.commit().map_err(sql_error)?;
            Ok(receipt)
        })
    }

    /// Applies a final usage fact to a finished execution without changing its outcome.
    /// # Errors
    /// Rejects unknown usage, foreign terminal identity, changed settlement replay, or storage failure.
    pub fn settle_reconciliation(
        &mut self,
        job_id: &ExecutionJobId,
        terminal_request_id: &RequestId,
        settlement_request_id: &RequestId,
        usage: &winwincode_execution_port::generated::ExecutionOutcomeUsage,
        observed_at: &Instant,
    ) -> Result<bool, ExecutionAdmissionError> {
        validate_id(&job_id.0, "job_", "jobId")?;
        validate_id(&terminal_request_id.0, "req_", "terminalRequestId")?;
        validate_id(&settlement_request_id.0, "req_", "settlementRequestId")?;
        validate_instant(observed_at)?;
        if !winwincode_execution_port::usage::valid_usage(usage) {
            return Err(ExecutionAdmissionError::invalid(
                "settlement usage is inconsistent",
            ));
        }
        let unresolved: bool = self
            .storage
            .connection()
            .map_err(storage_error)?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM execution_attempt_accounting_pending WHERE job_id=?1)",
                [&job_id.0],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if unresolved && (usage.tokens.is_some() || usage.cost_microunits.is_some()) {
            return Err(ExecutionAdmissionError::invalid(
                "predecessor attempt accounting remains unresolved",
            ));
        }
        let value = serde_json::to_string(usage)
            .map_err(|_| ExecutionAdmissionError::invalid("settlement encoding failed"))?;
        let tx = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let previous:Option<(String,String,String)>=tx.query_row("SELECT job_id,usage_json,observed_at FROM execution_admission_reconciliation_facts WHERE settlement_request_id=?1",[&settlement_request_id.0],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(sql_error)?;
        if let Some(previous) = previous {
            if previous != (job_id.0.clone(), value, observed_at.0.clone()) {
                return Err(ExecutionAdmissionError::invalid(
                    "settlement replay conflicts",
                ));
            }
            let bound:String=tx.query_row("SELECT terminal_request_id FROM execution_admission_reconciliation WHERE job_id=?1",[&job_id.0],|r|r.get(0)).map_err(sql_error)?;
            if bound != terminal_request_id.0 {
                return Err(ExecutionAdmissionError::invalid(
                    "foreign terminal identity",
                ));
            }
            return Ok(false);
        }
        let row:(String,String)=tx.query_row(
            "SELECT terminal_request_id,COALESCE((SELECT usage_json FROM execution_admission_reconciliation_facts f WHERE f.job_id=r.job_id ORDER BY sequence DESC LIMIT 1),terminal_usage_json) FROM execution_admission_reconciliation r WHERE job_id=?1",
            [&job_id.0], |r| Ok((r.get(0)?,r.get(1)?))).map_err(sql_error)?;
        let original: winwincode_execution_port::generated::ExecutionOutcomeUsage =
            serde_json::from_str(&row.1)
                .map_err(|_| ExecutionAdmissionError::invalid("corrupt terminal accounting"))?;
        if row.0 != terminal_request_id.0
            || usage.known_tokens < original.known_tokens
            || usage.runtime_millis != original.runtime_millis
            || original
                .tokens
                .is_some_and(|tokens| usage.tokens != Some(tokens))
            || original
                .cost_microunits
                .is_some_and(|cost| usage.cost_microunits != Some(cost))
        {
            return Err(ExecutionAdmissionError::invalid(
                "settlement changed known accounting or authority",
            ));
        }
        let changed = *usage != original;
        tx.execute("INSERT INTO execution_admission_reconciliation_facts(job_id,settlement_request_id,usage_json,observed_at) VALUES (?1,?2,?3,?4)",params![job_id.0,settlement_request_id.0,value,observed_at.0]).map_err(sql_error)?;
        if usage.tokens.is_some() && usage.cost_microunits.is_some() {
            tx.execute("UPDATE execution_admission_reconciliation SET accounting_status='settled',settlement_request_id=?2,settlement_usage_json=?3,observed_at=?4 WHERE job_id=?1 AND accounting_status='pending'",params![job_id.0,settlement_request_id.0,value,observed_at.0]).map_err(sql_error)?;
        }
        tx.commit().map_err(sql_error)?;
        Ok(changed)
    }

    /// Settles a running reservation using actual usage and releases the
    /// unused portion of its token/cost reservation.
    ///
    /// # Errors
    ///
    /// Rejects stale/non-running requests, actual usage above the reservation,
    /// conflicting replays, and `SQLite` failures.
    #[allow(clippy::too_many_lines)]
    pub fn settle(
        &mut self,
        request: &ExecutionReservationSettlement,
    ) -> Result<ExecutionAdmissionReceipt, ExecutionAdmissionError> {
        validate_settlement(request)?;
        let unresolved: bool = self
            .storage
            .connection()
            .map_err(storage_error)?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM execution_attempt_accounting_pending WHERE job_id=?1)",
                [&request.job_id.0],
                |r| r.get(0),
            )
            .map_err(sql_error)?;
        if unresolved {
            return Err(ExecutionAdmissionError::invalid(
                "predecessor attempt accounting remains unresolved",
            ));
        }
        let scope_key = reservation_scope_key(&request.scope, &request.worker_pool_id)?;
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()
            .map_err(storage_error)?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(receipt) = replay_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
        )? {
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let current = load_scoped_reservation(
            &transaction,
            &request.scope,
            &request.worker_pool_id,
            &request.job_id,
        )?
        .ok_or_else(|| ExecutionAdmissionError::invalid("execution reservation does not exist"))?;
        validate_revision_state_time(
            &current,
            request.expected_revision,
            ExecutionReservationState::Running,
            &request.completed_at,
        )?;
        if current
            .reserved_tokens
            .is_some_and(|limit| request.actual_tokens > limit)
            || current.reserved_cost_microunits.is_some_and(|limit| {
                request
                    .actual_cost_microunits
                    .is_some_and(|cost| cost > limit)
            })
            || current
                .runtime_limit_millis
                .is_some_and(|limit| request.actual_runtime_millis > limit)
        {
            return Err(ExecutionAdmissionError::invalid(
                "actual execution usage exceeds its reservation",
            ));
        }
        let changed = transaction
            .execute(
                "UPDATE execution_admission_reservations
                 SET state = 'settled', revision = revision + 1, updated_at = ?1,
                     terminal_at = ?1, actual_tokens = ?2,
                     actual_cost_microunits = ?3, actual_runtime_millis = ?4
                 WHERE job_id = ?5 AND revision = ?6 AND state = 'running'",
                params![
                    request.completed_at.0,
                    to_sql_integer(request.actual_tokens, "actualTokens")?,
                    request
                        .actual_cost_microunits
                        .map(|cost| to_sql_integer(cost, "actualCostMicrounits"))
                        .transpose()?,
                    to_sql_integer(request.actual_runtime_millis, "actualRuntimeMillis")?,
                    request.job_id.0,
                    to_sql_integer(request.expected_revision, "expectedRevision")?,
                ],
            )
            .map_err(sql_error)?;
        if changed != 1 {
            return Err(ExecutionAdmissionError::adapter(
                "execution settlement lost its transaction authority",
            ));
        }
        insert_settlement_source(&transaction, &current, request)?;
        if request.actual_cost_microunits.is_none() {
            let tokens = i64::try_from(request.actual_tokens)
                .map_err(|_| ExecutionAdmissionError::invalid("token range"))?;
            let usage=winwincode_execution_port::generated::ExecutionOutcomeUsage {
                tokens:Some(tokens),known_tokens:tokens,accounting_status:winwincode_execution_port::generated::ExecutionOutcomeUsageAccountingStatus::Known,
                runtime_millis:i64::try_from(request.actual_runtime_millis).map_err(|_| ExecutionAdmissionError::invalid("runtime range"))?,cost_microunits:None,
            };
            transaction.execute("INSERT INTO execution_admission_reconciliation(job_id,terminal_request_id,terminal_usage_json,accounting_status) VALUES (?1,?2,?3,'pending')",params![request.job_id.0,request.request_id.0,serde_json::to_string(&usage).map_err(|_| ExecutionAdmissionError::invalid("usage encoding"))?]).map_err(sql_error)?;
        }
        finish_mutation(
            &transaction,
            &scope_key,
            &request.scope,
            &request.worker_pool_id,
            &request.job_id,
            &request.request_id,
            &request_digest,
        )
        .and_then(|receipt| {
            transaction.commit().map_err(sql_error)?;
            Ok(receipt)
        })
    }

    /// Loads one reservation only for its exact scope and worker pool.
    ///
    /// # Errors
    ///
    /// Rejects malformed input and `SQLite` failures.
    pub fn load_reservation(
        &self,
        scope: &ExecutionQueueScope,
        worker_pool_id: &WorkerPoolId,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionReservationRecord>, ExecutionAdmissionError> {
        validate_scope(scope)?;
        validate_id(&worker_pool_id.0, "wpl_", "workerPoolId")?;
        validate_id(&job_id.0, "job_", "jobId")?;
        load_scoped_reservation(
            self.storage.connection().map_err(storage_error)?,
            scope,
            worker_pool_id,
            job_id,
        )
    }

    /// Loads the one durable reservation identified by its globally unique Job.
    ///
    /// This is the scheduler/Worker composition seam: tenant attribution and
    /// pool placement are returned from the stored admission receipt rather
    /// than being repeated by a caller that is about to claim the Job.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities, corrupt stored reservations, and
    /// `SQLite` failures.
    pub fn load_reservation_by_job(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionReservationRecord>, ExecutionAdmissionError> {
        validate_id(&job_id.0, "job_", "jobId")?;
        let stored = self
            .storage
            .connection()
            .map_err(storage_error)?
            .query_row(
                "SELECT job_id, organization_id, workspace_id, project_id, repository_id,
                        delivery_id, product_session_id, user_id, worker_pool_id, access_kind,
                        worktree_key, state, reserved_tokens, reserved_cost_microunits,
                        runtime_limit_millis, actual_tokens, actual_cost_microunits,
                        actual_runtime_millis, revision, submitted_at, updated_at,
                        started_at, terminal_at, release_reason
                 FROM execution_admission_reservations WHERE job_id = ?1",
                [&job_id.0],
                stored_reservation_row,
            )
            .optional()
            .map_err(sql_error)?;
        stored.map(complete_reservation).transpose()
    }

    /// Loads the immutable settlement source for one exact execution job.
    ///
    /// # Errors
    ///
    /// Rejects malformed identity, corrupt source facts, and `SQLite` failures.
    pub fn load_settlement_source(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<Option<WorkerSettlementSourceEntry>, ExecutionAdmissionError> {
        validate_id(&job_id.0, "job_", "jobId")?;
        let connection = self.storage.connection().map_err(storage_error)?;
        let source = load_settlement_source_by_job(connection, job_id)?;
        if let Some(source) = &source {
            require_source_matches_reservation(connection, source)?;
        }
        Ok(source)
    }

    /// Scans immutable settlement sources using a bounded fixed snapshot.
    ///
    /// # Errors
    ///
    /// Rejects invalid cursors/limits, corrupt source facts, and `SQLite` failures.
    pub fn scan_settlement_sources(
        &self,
        cursor: Option<&WorkerSettlementSourceCursor>,
        limit: u64,
    ) -> Result<WorkerSettlementSourcePage, ExecutionAdmissionError> {
        scan_settlement_sources(
            self.storage.connection().map_err(storage_error)?,
            cursor,
            limit,
        )
    }

    /// Reads current queue, concurrency, reserved, and committed budget usage.
    ///
    /// # Errors
    ///
    /// Rejects malformed/missing policies and `SQLite` failures.
    pub fn usage(
        &self,
        boundary: &ExecutionAdmissionBoundary,
    ) -> Result<ExecutionAdmissionUsage, ExecutionAdmissionError> {
        validate_boundary(boundary)?;
        let connection = self.storage.connection().map_err(storage_error)?;
        let (key, _) = required_policy(connection, boundary)?;
        load_usage(connection, &key)
    }
}

fn ensure_reservation_capacity(
    connection: &Connection,
    request: &ExecutionReservationRequest,
    boundaries: &[ExecutionAdmissionBoundary],
) -> Result<(), ExecutionAdmissionError> {
    for boundary in boundaries {
        let (key, policy) = required_policy(connection, boundary)?;
        if policy.limits.max_runtime_millis.is_some_and(|limit| {
            request
                .runtime_limit_millis
                .is_none_or(|requested| requested > limit)
        }) {
            return Err(ExecutionAdmissionError::rejected(
                ExecutionAdmissionErrorCode::RuntimeLimitExceeded,
                boundary,
                "execution runtime limit exceeds the configured boundary",
            ));
        }
        let usage = load_usage(connection, &key)?;
        if usage.queued >= policy.limits.max_queued {
            return Err(ExecutionAdmissionError::rejected(
                ExecutionAdmissionErrorCode::QueueCapacityExhausted,
                boundary,
                "execution queue capacity is exhausted",
            ));
        }
        if policy.limits.token_budget.is_some_and(|limit| {
            usage.accounting_pending > 0
                || request.reserved_tokens.is_none_or(|requested| {
                    total_tokens(&usage) + u128::from(requested) > u128::from(limit)
                })
        }) {
            return Err(ExecutionAdmissionError::rejected(
                ExecutionAdmissionErrorCode::TokenBudgetExhausted,
                boundary,
                "execution token budget is exhausted",
            ));
        }
        if policy.limits.cost_budget_microunits.is_some_and(|limit| {
            request.reserved_cost_microunits.is_none_or(|requested| {
                total_cost(&usage)
                    .is_none_or(|cost| cost + u128::from(requested) > u128::from(limit))
            })
        }) {
            return Err(ExecutionAdmissionError::rejected(
                ExecutionAdmissionErrorCode::CostBudgetExhausted,
                boundary,
                "execution cost budget is exhausted",
            ));
        }
    }
    Ok(())
}

fn insert_reservation(
    connection: &Connection,
    scope_key: &str,
    request: &ExecutionReservationRequest,
    boundaries: &[ExecutionAdmissionBoundary],
) -> Result<(), ExecutionAdmissionError> {
    let (access_kind, worktree_key) = access_columns(&request.repository_access);
    connection
        .execute(
            "INSERT INTO execution_admission_reservations
                (job_id, scope_key, organization_id, workspace_id, project_id,
                 repository_id, delivery_id, product_session_id, user_id, worker_pool_id,
                 access_kind, worktree_key, state, reserved_tokens,
                 reserved_cost_microunits, runtime_limit_millis, actual_tokens,
                 actual_cost_microunits, actual_runtime_millis, revision,
                 submitted_at, updated_at, started_at, terminal_at, release_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'queued',
                     ?13, ?14, ?15, NULL, NULL, NULL, 1, ?16, ?16, NULL, NULL, NULL)",
            params![
                request.job_id.0,
                scope_key,
                request.scope.organization_id.0,
                request.scope.workspace_id.0,
                request.scope.project_id.0,
                request.scope.repository_id.0,
                request
                    .scope
                    .delivery_id
                    .as_ref()
                    .map(|delivery| delivery.0.as_str()),
                request.scope.product_session_id.0,
                request.user_id.0,
                request.worker_pool_id.0,
                access_kind,
                worktree_key,
                request
                    .reserved_tokens
                    .map(|value| to_sql_integer(value, "reservedTokens"))
                    .transpose()?,
                request
                    .reserved_cost_microunits
                    .map(|value| to_sql_integer(value, "reservedCostMicrounits"))
                    .transpose()?,
                request
                    .runtime_limit_millis
                    .map(|value| to_sql_integer(value, "runtimeLimitMillis"))
                    .transpose()?,
                request.submitted_at.0,
            ],
        )
        .map_err(sql_error)?;
    for (ordinal, boundary) in boundaries.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO execution_admission_reservation_boundaries
                    (job_id, boundary_key, ordinal) VALUES (?1, ?2, ?3)",
                params![
                    request.job_id.0,
                    boundary_key(boundary)?,
                    i64::try_from(ordinal).map_err(|_| {
                        ExecutionAdmissionError::invalid("admission boundary ordinal is invalid")
                    })?,
                ],
            )
            .map_err(sql_error)?;
    }
    Ok(())
}

fn request_boundaries(
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
) -> Vec<ExecutionAdmissionBoundary> {
    let mut boundaries = vec![
        ExecutionAdmissionBoundary::Organization {
            organization_id: scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
    ];
    if let Some(delivery_id) = &scope.delivery_id {
        boundaries.push(ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        });
    }
    boundaries.extend([
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        },
    ]);
    boundaries
}

fn required_policy(
    connection: &Connection,
    boundary: &ExecutionAdmissionBoundary,
) -> Result<(String, ExecutionAdmissionPolicy), ExecutionAdmissionError> {
    let key = boundary_key(boundary)?;
    let stored = connection
        .query_row(
            "SELECT boundary_json, max_concurrent, max_queued, token_budget,
                    cost_budget_microunits, max_runtime_millis
             FROM execution_admission_policies WHERE boundary_key = ?1",
            [&key],
            stored_policy_row,
        )
        .optional()
        .map_err(sql_error)?;
    let policy = stored
        .as_ref()
        .map(complete_policy)
        .transpose()?
        .ok_or_else(|| {
            ExecutionAdmissionError::rejected(
                ExecutionAdmissionErrorCode::PolicyMissing,
                boundary,
                "execution admission policy is missing",
            )
        })?;
    if policy.boundary != *boundary {
        return Err(ExecutionAdmissionError::adapter(
            "stored admission policy boundary digest does not match",
        ));
    }
    Ok((key, policy))
}

struct StoredPolicyRow {
    boundary_json: String,
    max_concurrent: i64,
    max_queued: i64,
    token_budget: Option<i64>,
    cost_budget_microunits: Option<i64>,
    max_runtime_millis: Option<i64>,
}

fn stored_policy_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredPolicyRow> {
    Ok(StoredPolicyRow {
        boundary_json: row.get(0)?,
        max_concurrent: row.get(1)?,
        max_queued: row.get(2)?,
        token_budget: row.get(3)?,
        cost_budget_microunits: row.get(4)?,
        max_runtime_millis: row.get(5)?,
    })
}

fn complete_policy(
    stored: &StoredPolicyRow,
) -> Result<ExecutionAdmissionPolicy, ExecutionAdmissionError> {
    let policy = ExecutionAdmissionPolicy {
        boundary: decode_json(&stored.boundary_json)?,
        limits: ExecutionAdmissionLimits {
            max_concurrent: from_sql_integer(stored.max_concurrent, "maxConcurrent")?,
            max_queued: from_sql_integer(stored.max_queued, "maxQueued")?,
            token_budget: stored
                .token_budget
                .map(|value| from_sql_integer(value, "tokenBudget"))
                .transpose()?,
            cost_budget_microunits: stored
                .cost_budget_microunits
                .map(|value| from_sql_integer(value, "costBudgetMicrounits"))
                .transpose()?,
            max_runtime_millis: stored
                .max_runtime_millis
                .map(|value| from_sql_integer(value, "maxRuntimeMillis"))
                .transpose()?,
        },
    };
    validate_policy(&policy)
        .map_err(|_| ExecutionAdmissionError::adapter("stored admission policy is invalid"))?;
    Ok(policy)
}

#[allow(clippy::too_many_lines)]
fn load_usage(
    connection: &Connection,
    boundary_key: &str,
) -> Result<ExecutionAdmissionUsage, ExecutionAdmissionError> {
    let mut statement = connection
        .prepare(
            "SELECT r.state, r.reserved_tokens, r.reserved_cost_microunits,
                    r.actual_tokens, r.actual_cost_microunits,
                    (SELECT COALESCE((SELECT usage_json FROM execution_admission_reconciliation_facts f WHERE f.job_id=r.job_id ORDER BY sequence DESC LIMIT 1),a.terminal_usage_json) FROM execution_admission_reconciliation a WHERE a.job_id=r.job_id)
             FROM execution_admission_reservation_boundaries b
             JOIN execution_admission_reservations r ON r.job_id = b.job_id
             WHERE b.boundary_key = ?1",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map([boundary_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(sql_error)?;
    let mut usage = ExecutionAdmissionUsage::default();
    for row in rows {
        let (state, reserved_tokens, reserved_cost, actual_tokens, actual_cost, reconciliation) =
            row.map_err(sql_error)?;
        if let Some(fact) = reconciliation {
            let accounting: winwincode_execution_port::generated::ExecutionOutcomeUsage =
                serde_json::from_str(&fact).map_err(|_| {
                    ExecutionAdmissionError::invalid("corrupt reconciliation usage")
                })?;
            if !winwincode_execution_port::usage::valid_usage(&accounting)
                || !matches!(state.as_str(), "released" | "settled")
            {
                return Err(ExecutionAdmissionError::invalid(
                    "inconsistent reconciliation authority",
                ));
            }
            usage.committed_tokens = checked_add(
                usage.committed_tokens,
                u64::try_from(accounting.known_tokens)
                    .map_err(|_| ExecutionAdmissionError::invalid("usage range"))?,
                "reconciled tokens",
            )?;
            if accounting.tokens.is_none() {
                usage.accounting_pending =
                    checked_add(usage.accounting_pending, 1, "pending accounting")?;
            }
            usage.committed_cost_microunits = usage
                .committed_cost_microunits
                .zip(accounting.cost_microunits)
                .map(|(a, b)| {
                    checked_add(
                        a,
                        u64::try_from(b)
                            .map_err(|_| ExecutionAdmissionError::invalid("cost range"))?,
                        "reconciled cost",
                    )
                })
                .transpose()?;
            continue;
        }
        match ExecutionReservationState::parse(&state)? {
            ExecutionReservationState::Queued => {
                usage.queued = checked_add(usage.queued, 1, "queued usage")?;
                usage.reserved_tokens = checked_add(
                    usage.reserved_tokens,
                    reserved_tokens
                        .map(|value| from_sql_integer(value, "reservedTokens"))
                        .transpose()?
                        .unwrap_or(0),
                    "reserved token usage",
                )?;
                usage.reserved_cost_microunits = checked_add(
                    usage.reserved_cost_microunits,
                    reserved_cost
                        .map(|value| from_sql_integer(value, "reservedCostMicrounits"))
                        .transpose()?
                        .unwrap_or(0),
                    "reserved cost usage",
                )?;
            }
            ExecutionReservationState::Running => {
                usage.running = checked_add(usage.running, 1, "running usage")?;
                usage.reserved_tokens = checked_add(
                    usage.reserved_tokens,
                    reserved_tokens
                        .map(|value| from_sql_integer(value, "reservedTokens"))
                        .transpose()?
                        .unwrap_or(0),
                    "reserved token usage",
                )?;
                usage.reserved_cost_microunits = checked_add(
                    usage.reserved_cost_microunits,
                    reserved_cost
                        .map(|value| from_sql_integer(value, "reservedCostMicrounits"))
                        .transpose()?
                        .unwrap_or(0),
                    "reserved cost usage",
                )?;
            }
            ExecutionReservationState::Settled => {
                usage.committed_tokens = checked_add(
                    usage.committed_tokens,
                    from_sql_integer(
                        actual_tokens.ok_or_else(|| {
                            ExecutionAdmissionError::adapter(
                                "settled reservation has no actual token usage",
                            )
                        })?,
                        "actualTokens",
                    )?,
                    "committed token usage",
                )?;
                usage.committed_cost_microunits = usage
                    .committed_cost_microunits
                    .zip(actual_cost)
                    .map(|(known, cost)| {
                        checked_add(
                            known,
                            from_sql_integer(cost, "actualCostMicrounits")?,
                            "committed cost usage",
                        )
                    })
                    .transpose()?;
            }
            ExecutionReservationState::Released => {}
        }
    }
    let pending:i64=connection.query_row("SELECT COUNT(*) FROM execution_attempt_accounting_pending p JOIN execution_admission_reservation_boundaries b ON b.job_id=p.job_id JOIN execution_admission_reservations r ON r.job_id=p.job_id WHERE b.boundary_key=?1 ",[boundary_key],|r|r.get(0)).map_err(sql_error)?;
    if pending > 0 {
        usage.accounting_pending = checked_add(
            usage.accounting_pending,
            u64::try_from(pending)
                .map_err(|_| ExecutionAdmissionError::invalid("pending accounting range"))?,
            "predecessor accounting",
        )?;
        usage.committed_cost_microunits = None;
    }
    Ok(usage)
}

fn total_tokens(usage: &ExecutionAdmissionUsage) -> u128 {
    u128::from(usage.reserved_tokens) + u128::from(usage.committed_tokens)
}

fn total_cost(usage: &ExecutionAdmissionUsage) -> Option<u128> {
    usage
        .committed_cost_microunits
        .map(|cost| u128::from(usage.reserved_cost_microunits) + u128::from(cost))
}

fn checked_add(left: u64, right: u64, field: &str) -> Result<u64, ExecutionAdmissionError> {
    left.checked_add(right).ok_or_else(|| {
        ExecutionAdmissionError::adapter(format!("stored {field} exceeds the supported range"))
    })
}

fn has_repository_write_conflict(
    connection: &Connection,
    current: &ExecutionReservationRecord,
) -> Result<bool, ExecutionAdmissionError> {
    if current.repository_access == ExecutionRepositoryAccess::ReadOnly {
        return Ok(false);
    }
    let mut statement = connection
        .prepare(
            "SELECT access_kind, worktree_key
             FROM execution_admission_reservations
             WHERE organization_id = ?1 AND project_id = ?2 AND repository_id = ?3
               AND state = 'running' AND access_kind != 'read_only' AND job_id != ?4",
        )
        .map_err(sql_error)?;
    let writes = statement
        .query_map(
            params![
                current.scope.organization_id.0,
                current.scope.project_id.0,
                current.scope.repository_id.0,
                current.job_id.0,
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .map_err(sql_error)?;
    for write in writes {
        let (access_kind, worktree_key) = write.map_err(sql_error)?;
        if match &current.repository_access {
            ExecutionRepositoryAccess::ReadOnly => false,
            ExecutionRepositoryAccess::SharedWrite => true,
            ExecutionRepositoryAccess::IsolatedWrite {
                worktree_key: current_key,
            } => access_kind == "shared_write" || worktree_key.as_ref() == Some(current_key),
        } {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn transition_reservation(
    connection: &Connection,
    job_id: &ExecutionJobId,
    expected_revision: u64,
    from: ExecutionReservationState,
    to: ExecutionReservationState,
    updated_at: &Instant,
    started_at: Option<&Instant>,
    terminal_at: Option<&Instant>,
    release_reason: Option<ExecutionReservationReleaseReason>,
) -> Result<(), ExecutionAdmissionError> {
    let changed = connection
        .execute(
            "UPDATE execution_admission_reservations
             SET state = ?1, revision = revision + 1, updated_at = ?2,
                 started_at = ?3, terminal_at = ?4, release_reason = ?5
             WHERE job_id = ?6 AND revision = ?7 AND state = ?8",
            params![
                to.as_str(),
                updated_at.0,
                started_at.map(|value| value.0.as_str()),
                terminal_at.map(|value| value.0.as_str()),
                release_reason.map(ExecutionReservationReleaseReason::as_str),
                job_id.0,
                to_sql_integer(expected_revision, "expectedRevision")?,
                from.as_str(),
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(ExecutionAdmissionError::adapter(
            "execution reservation transition lost its transaction authority",
        ));
    }
    Ok(())
}

fn finish_mutation(
    connection: &Connection,
    scope_key: &str,
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
    job_id: &ExecutionJobId,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<ExecutionAdmissionReceipt, ExecutionAdmissionError> {
    let reservation = load_scoped_reservation(connection, scope, worker_pool_id, job_id)?
        .ok_or_else(|| {
            ExecutionAdmissionError::adapter("mutated execution reservation was not stored")
        })?;
    let receipt = ExecutionAdmissionReceipt {
        reservation,
        replayed: false,
    };
    insert_receipt(connection, scope_key, request_id, request_digest, &receipt)?;
    Ok(receipt)
}

fn insert_settlement_source(
    connection: &Connection,
    reservation: &ExecutionReservationRecord,
    request: &ExecutionReservationSettlement,
) -> Result<WorkerSettlementSourceEntry, ExecutionAdmissionError> {
    let fact = WorkerSettlementSourceFact {
        job_id: reservation.job_id.clone(),
        settlement_request_id: request.request_id.clone(),
        worker_pool_id: reservation.worker_pool_id.clone(),
        scope: reservation.scope.clone(),
        user_id: reservation.user_id.clone(),
        actual_runtime_millis: request.actual_runtime_millis,
        actual_tokens: request.actual_tokens,
        actual_cost_microunits: request.actual_cost_microunits,
        completed_at: request.completed_at.clone(),
    };
    validate_settlement_source_fact(&fact)?;
    let source_key = settlement_source_key(&fact)?;
    let source_digest = settlement_source_digest(&fact)?;
    let fact_json = encode_json(&fact)?;
    connection
        .execute(
            "INSERT INTO execution_admission_settlement_sources
                (source_key, source_digest, fact_json, job_id, settlement_request_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                source_key,
                source_digest.0,
                fact_json,
                fact.job_id.0,
                fact.settlement_request_id.0,
            ],
        )
        .map_err(sql_error)?;
    let sequence = u64::try_from(connection.last_insert_rowid()).map_err(|_| {
        ExecutionAdmissionError::adapter("worker settlement source sequence is invalid")
    })?;
    Ok(WorkerSettlementSourceEntry {
        sequence,
        source_digest,
        fact,
    })
}

#[derive(Debug)]
struct StoredSettlementSourceRow {
    sequence: i64,
    source_key: String,
    source_digest: String,
    fact_json: String,
    job_id: String,
    settlement_request_id: String,
}

fn stored_settlement_source_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StoredSettlementSourceRow> {
    Ok(StoredSettlementSourceRow {
        sequence: row.get(0)?,
        source_key: row.get(1)?,
        source_digest: row.get(2)?,
        fact_json: row.get(3)?,
        job_id: row.get(4)?,
        settlement_request_id: row.get(5)?,
    })
}

fn complete_settlement_source(
    stored: StoredSettlementSourceRow,
) -> Result<WorkerSettlementSourceEntry, ExecutionAdmissionError> {
    let sequence = from_sql_integer(stored.sequence, "settlementSourceSequence")?;
    if sequence == 0 {
        return Err(ExecutionAdmissionError::adapter(
            "worker settlement source sequence is invalid",
        ));
    }
    let fact: WorkerSettlementSourceFact = decode_json(&stored.fact_json)?;
    validate_settlement_source_fact(&fact).map_err(|_| {
        ExecutionAdmissionError::adapter("stored worker settlement source fact is invalid")
    })?;
    let source_digest = Sha256Digest(stored.source_digest);
    if stored.source_key != settlement_source_key(&fact)?
        || source_digest != settlement_source_digest(&fact)?
        || stored.fact_json != encode_json(&fact)?
        || stored.job_id != fact.job_id.0
        || stored.settlement_request_id != fact.settlement_request_id.0
    {
        return Err(ExecutionAdmissionError::adapter(
            "stored worker settlement source differs from its canonical fact",
        ));
    }
    Ok(WorkerSettlementSourceEntry {
        sequence,
        source_digest,
        fact,
    })
}

fn load_settlement_source_by_job(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<WorkerSettlementSourceEntry>, ExecutionAdmissionError> {
    connection
        .query_row(
            "SELECT sequence, source_key, source_digest, fact_json, job_id,
                    settlement_request_id
             FROM execution_admission_settlement_sources WHERE job_id = ?1",
            [&job_id.0],
            stored_settlement_source_row,
        )
        .optional()
        .map_err(sql_error)?
        .map(complete_settlement_source)
        .transpose()
}

fn require_source_matches_reservation(
    connection: &Connection,
    source: &WorkerSettlementSourceEntry,
) -> Result<(), ExecutionAdmissionError> {
    let reservation = load_scoped_reservation(
        connection,
        &source.fact.scope,
        &source.fact.worker_pool_id,
        &source.fact.job_id,
    )?
    .ok_or_else(|| {
        ExecutionAdmissionError::adapter(
            "worker settlement source does not identify its execution reservation",
        )
    })?;
    let matches = reservation.state == ExecutionReservationState::Settled
        && reservation.user_id == source.fact.user_id
        && reservation.actual_runtime_millis == Some(source.fact.actual_runtime_millis)
        && reservation.actual_tokens == Some(source.fact.actual_tokens)
        && reservation.actual_cost_microunits == source.fact.actual_cost_microunits
        && reservation.terminal_at.as_ref() == Some(&source.fact.completed_at);
    if !matches {
        return Err(ExecutionAdmissionError::adapter(
            "worker settlement source differs from its settled execution reservation",
        ));
    }
    let scope_key = reservation_scope_key(&source.fact.scope, &source.fact.worker_pool_id)?;
    let response_json = connection
        .query_row(
            "SELECT response_json FROM execution_admission_receipts
             WHERE scope_key = ?1 AND request_id = ?2",
            params![scope_key, source.fact.settlement_request_id.0],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error)?
        .ok_or_else(|| {
            ExecutionAdmissionError::adapter(
                "worker settlement source does not identify its settlement receipt",
            )
        })?;
    let receipt: ExecutionAdmissionReceipt = decode_json(&response_json)?;
    if receipt.replayed || receipt.reservation != reservation {
        return Err(ExecutionAdmissionError::adapter(
            "worker settlement source differs from its settlement receipt",
        ));
    }
    Ok(())
}

fn scan_settlement_sources(
    connection: &Connection,
    cursor: Option<&WorkerSettlementSourceCursor>,
    limit: u64,
) -> Result<WorkerSettlementSourcePage, ExecutionAdmissionError> {
    if limit == 0 || limit > MAX_SETTLEMENT_SOURCE_PAGE_SIZE {
        return Err(ExecutionAdmissionError::invalid(
            "worker settlement source page limit is outside 1..=200",
        ));
    }
    let (snapshot_sequence, after_sequence) = settlement_source_snapshot(connection, cursor)?;
    let query_limit = limit.checked_add(1).ok_or_else(|| {
        ExecutionAdmissionError::invalid("worker settlement source page limit overflowed")
    })?;
    let mut statement = connection
        .prepare(
            "SELECT sequence, source_key, source_digest, fact_json, job_id,
                    settlement_request_id
             FROM execution_admission_settlement_sources
             WHERE sequence > ?1 AND sequence <= ?2
             ORDER BY sequence LIMIT ?3",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(
            params![
                to_sql_integer(after_sequence, "settlementSourceCursor")?,
                to_sql_integer(snapshot_sequence, "settlementSourceSnapshot")?,
                to_sql_integer(query_limit, "settlementSourcePageLimit")?,
            ],
            stored_settlement_source_row,
        )
        .map_err(sql_error)?;
    let mut entries = rows
        .map(|row| row.map_err(sql_error).and_then(complete_settlement_source))
        .collect::<Result<Vec<_>, _>>()?;
    for entry in &entries {
        require_source_matches_reservation(connection, entry)?;
    }
    let page_size = usize::try_from(limit).map_err(|_| {
        ExecutionAdmissionError::invalid("worker settlement source page limit is invalid")
    })?;
    let has_more = entries.len() > page_size;
    if has_more {
        entries.pop();
    }
    let next = if has_more {
        Some(WorkerSettlementSourceCursor {
            snapshot_sequence,
            after_sequence: entries
                .last()
                .ok_or_else(|| {
                    ExecutionAdmissionError::adapter(
                        "worker settlement source page is unexpectedly empty",
                    )
                })?
                .sequence,
        })
    } else {
        None
    };
    Ok(WorkerSettlementSourcePage {
        snapshot_sequence,
        entries,
        next,
    })
}

fn settlement_source_snapshot(
    connection: &Connection,
    cursor: Option<&WorkerSettlementSourceCursor>,
) -> Result<(u64, u64), ExecutionAdmissionError> {
    match cursor {
        Some(cursor)
            if cursor.snapshot_sequence <= MAX_SAFE_INTEGER
                && cursor.after_sequence <= cursor.snapshot_sequence =>
        {
            Ok((cursor.snapshot_sequence, cursor.after_sequence))
        }
        Some(_) => Err(ExecutionAdmissionError::invalid(
            "worker settlement source cursor does not identify a valid snapshot",
        )),
        None => {
            let sequence = connection
                .query_row(
                    "SELECT COALESCE(MAX(sequence), 0)
                     FROM execution_admission_settlement_sources",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sql_error)?;
            Ok((from_sql_integer(sequence, "settlementSourceSnapshot")?, 0))
        }
    }
}

fn validate_settlement_source_fact(
    fact: &WorkerSettlementSourceFact,
) -> Result<(), ExecutionAdmissionError> {
    validate_scope(&fact.scope)?;
    validate_id(&fact.user_id.0, "usr_", "userId")?;
    validate_id(&fact.worker_pool_id.0, "wpl_", "workerPoolId")?;
    validate_id(&fact.job_id.0, "job_", "jobId")?;
    validate_id(&fact.settlement_request_id.0, "req_", "settlementRequestId")?;
    validate_sql_range(fact.actual_runtime_millis, "actualRuntimeMillis")?;
    validate_sql_range(fact.actual_tokens, "actualTokens")?;
    if let Some(cost) = fact.actual_cost_microunits {
        validate_sql_range(cost, "actualCostMicrounits")?;
    }
    validate_instant(&fact.completed_at)
}

fn settlement_source_key(
    fact: &WorkerSettlementSourceFact,
) -> Result<String, ExecutionAdmissionError> {
    digest(&(
        &fact.job_id,
        &fact.settlement_request_id,
        &fact.worker_pool_id,
    ))
}

fn settlement_source_digest(
    fact: &WorkerSettlementSourceFact,
) -> Result<Sha256Digest, ExecutionAdmissionError> {
    Ok(Sha256Digest(digest(fact)?))
}

fn reservation_exists(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<bool, ExecutionAdmissionError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM execution_admission_reservations WHERE job_id = ?1",
            [&job_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some())
}

fn replay_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<Option<ExecutionAdmissionReceipt>, ExecutionAdmissionError> {
    let stored = connection
        .query_row(
            "SELECT request_digest, response_json FROM execution_admission_receipts
             WHERE scope_key = ?1 AND request_id = ?2",
            params![scope_key, request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((stored_digest, response_json)) = stored else {
        return Ok(None);
    };
    if stored_digest != request_digest {
        return Err(ExecutionAdmissionError::new(
            ExecutionAdmissionErrorCode::RequestConflict,
            None,
            "execution admission request id was reused with a different body",
        ));
    }
    let mut receipt: ExecutionAdmissionReceipt = decode_json(&response_json)?;
    validate_stored_reservation(&receipt.reservation)?;
    receipt.replayed = true;
    Ok(Some(receipt))
}

fn insert_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    request_digest: &str,
    receipt: &ExecutionAdmissionReceipt,
) -> Result<(), ExecutionAdmissionError> {
    connection
        .execute(
            "INSERT INTO execution_admission_receipts
                (scope_key, request_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                scope_key,
                request_id.0,
                request_digest,
                encode_json(receipt)?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

#[allow(clippy::struct_field_names)]
struct StoredReservationRow {
    job_id: String,
    organization_id: String,
    workspace_id: String,
    project_id: String,
    repository_id: String,
    delivery_id: Option<String>,
    product_session_id: String,
    user_id: String,
    worker_pool_id: String,
    access_kind: String,
    worktree_key: Option<String>,
    state: String,
    reserved_tokens: Option<i64>,
    reserved_cost_microunits: Option<i64>,
    runtime_limit_millis: Option<i64>,
    actual_tokens: Option<i64>,
    actual_cost_microunits: Option<i64>,
    actual_runtime_millis: Option<i64>,
    revision: i64,
    submitted_at: String,
    updated_at: String,
    started_at: Option<String>,
    terminal_at: Option<String>,
    release_reason: Option<String>,
}

fn stored_reservation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredReservationRow> {
    Ok(StoredReservationRow {
        job_id: row.get(0)?,
        organization_id: row.get(1)?,
        workspace_id: row.get(2)?,
        project_id: row.get(3)?,
        repository_id: row.get(4)?,
        delivery_id: row.get(5)?,
        product_session_id: row.get(6)?,
        user_id: row.get(7)?,
        worker_pool_id: row.get(8)?,
        access_kind: row.get(9)?,
        worktree_key: row.get(10)?,
        state: row.get(11)?,
        reserved_tokens: row.get(12)?,
        reserved_cost_microunits: row.get(13)?,
        runtime_limit_millis: row.get(14)?,
        actual_tokens: row.get(15)?,
        actual_cost_microunits: row.get(16)?,
        actual_runtime_millis: row.get(17)?,
        revision: row.get(18)?,
        submitted_at: row.get(19)?,
        updated_at: row.get(20)?,
        started_at: row.get(21)?,
        terminal_at: row.get(22)?,
        release_reason: row.get(23)?,
    })
}

fn load_scoped_reservation(
    connection: &Connection,
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionReservationRecord>, ExecutionAdmissionError> {
    let stored = connection
        .query_row(
            "SELECT job_id, organization_id, workspace_id, project_id, repository_id,
                    delivery_id, product_session_id, user_id, worker_pool_id, access_kind,
                    worktree_key, state, reserved_tokens, reserved_cost_microunits,
                    runtime_limit_millis, actual_tokens, actual_cost_microunits,
                    actual_runtime_millis, revision, submitted_at, updated_at,
                    started_at, terminal_at, release_reason
             FROM execution_admission_reservations
             WHERE job_id = ?1 AND organization_id = ?2 AND workspace_id = ?3
               AND project_id = ?4 AND repository_id = ?5 AND delivery_id IS ?6
               AND product_session_id = ?7 AND worker_pool_id = ?8",
            params![
                job_id.0,
                scope.organization_id.0,
                scope.workspace_id.0,
                scope.project_id.0,
                scope.repository_id.0,
                scope.delivery_id.as_ref().map(|value| value.0.as_str()),
                scope.product_session_id.0,
                worker_pool_id.0,
            ],
            stored_reservation_row,
        )
        .optional()
        .map_err(sql_error)?;
    stored.map(complete_reservation).transpose()
}

fn complete_reservation(
    stored: StoredReservationRow,
) -> Result<ExecutionReservationRecord, ExecutionAdmissionError> {
    let repository_access = match (stored.access_kind.as_str(), stored.worktree_key) {
        ("read_only", None) => ExecutionRepositoryAccess::ReadOnly,
        ("shared_write", None) => ExecutionRepositoryAccess::SharedWrite,
        ("isolated_write", Some(worktree_key)) => {
            ExecutionRepositoryAccess::IsolatedWrite { worktree_key }
        }
        _ => {
            return Err(ExecutionAdmissionError::adapter(
                "stored repository access is invalid",
            ));
        }
    };
    let reservation = ExecutionReservationRecord {
        scope: ExecutionQueueScope {
            organization_id: OrganizationId(stored.organization_id),
            workspace_id: WorkspaceId(stored.workspace_id),
            project_id: ProjectId(stored.project_id),
            repository_id: RepositoryId(stored.repository_id),
            delivery_id: stored.delivery_id.map(DeliveryId),
            product_session_id: ProductSessionId(stored.product_session_id),
        },
        user_id: UserId(stored.user_id),
        worker_pool_id: WorkerPoolId(stored.worker_pool_id),
        job_id: ExecutionJobId(stored.job_id),
        repository_access,
        state: ExecutionReservationState::parse(&stored.state)?,
        reserved_tokens: stored
            .reserved_tokens
            .map(|value| from_sql_integer(value, "reservedTokens"))
            .transpose()?,
        reserved_cost_microunits: stored
            .reserved_cost_microunits
            .map(|value| from_sql_integer(value, "reservedCostMicrounits"))
            .transpose()?,
        runtime_limit_millis: stored
            .runtime_limit_millis
            .map(|value| from_sql_integer(value, "runtimeLimitMillis"))
            .transpose()?,
        actual_tokens: stored
            .actual_tokens
            .map(|value| from_sql_integer(value, "actualTokens"))
            .transpose()?,
        actual_cost_microunits: stored
            .actual_cost_microunits
            .map(|value| from_sql_integer(value, "actualCostMicrounits"))
            .transpose()?,
        actual_runtime_millis: stored
            .actual_runtime_millis
            .map(|value| from_sql_integer(value, "actualRuntimeMillis"))
            .transpose()?,
        revision: from_sql_integer(stored.revision, "revision")?,
        submitted_at: Instant(stored.submitted_at),
        updated_at: Instant(stored.updated_at),
        started_at: stored.started_at.map(Instant),
        terminal_at: stored.terminal_at.map(Instant),
        release_reason: stored
            .release_reason
            .map(|value| ExecutionReservationReleaseReason::parse(&value))
            .transpose()?,
    };
    validate_stored_reservation(&reservation)?;
    Ok(reservation)
}

fn validate_stored_reservation(
    reservation: &ExecutionReservationRecord,
) -> Result<(), ExecutionAdmissionError> {
    validate_scope(&reservation.scope)
        .map_err(|_| ExecutionAdmissionError::adapter("stored reservation scope is invalid"))?;
    validate_id(&reservation.worker_pool_id.0, "wpl_", "workerPoolId")
        .map_err(|_| ExecutionAdmissionError::adapter("stored worker pool is invalid"))?;
    validate_id(&reservation.job_id.0, "job_", "jobId")
        .map_err(|_| ExecutionAdmissionError::adapter("stored reservation job is invalid"))?;
    validate_id(&reservation.user_id.0, "usr_", "userId")
        .map_err(|_| ExecutionAdmissionError::adapter("stored reservation user is invalid"))?;
    validate_access(&reservation.repository_access)
        .map_err(|_| ExecutionAdmissionError::adapter("stored repository access is invalid"))?;
    for instant in [
        Some(&reservation.submitted_at),
        Some(&reservation.updated_at),
        reservation.started_at.as_ref(),
        reservation.terminal_at.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_instant(instant)
            .map_err(|_| ExecutionAdmissionError::adapter("stored reservation time is invalid"))?;
    }
    if reservation.revision == 0
        || reservation.runtime_limit_millis == Some(0)
        || reservation.updated_at.0 < reservation.submitted_at.0
        || reservation
            .started_at
            .as_ref()
            .is_some_and(|value| value.0 < reservation.submitted_at.0)
        || reservation
            .terminal_at
            .as_ref()
            .is_some_and(|value| value.0 < reservation.updated_at.0)
    {
        return Err(ExecutionAdmissionError::adapter(
            "stored reservation values are invalid",
        ));
    }
    let valid_terminal = match reservation.state {
        ExecutionReservationState::Queued => {
            reservation.started_at.is_none()
                && reservation.terminal_at.is_none()
                && reservation.release_reason.is_none()
                && reservation.actual_tokens.is_none()
        }
        ExecutionReservationState::Running => {
            reservation.started_at.is_some()
                && reservation.terminal_at.is_none()
                && reservation.release_reason.is_none()
                && reservation.actual_tokens.is_none()
        }
        ExecutionReservationState::Released => {
            reservation.terminal_at.is_some()
                && reservation.release_reason.is_some()
                && reservation.actual_tokens.is_none()
        }
        ExecutionReservationState::Settled => {
            reservation.started_at.is_some()
                && reservation.terminal_at.is_some()
                && reservation.release_reason.is_none()
                && reservation.actual_tokens.is_some()
                && reservation.actual_runtime_millis.is_some()
        }
    };
    if !valid_terminal {
        return Err(ExecutionAdmissionError::adapter(
            "stored reservation lifecycle fields are inconsistent",
        ));
    }
    Ok(())
}

fn validate_reservation_request(
    request: &ExecutionReservationRequest,
) -> Result<(), ExecutionAdmissionError> {
    validate_scope(&request.scope)?;
    validate_id(&request.worker_pool_id.0, "wpl_", "workerPoolId")?;
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_id(&request.user_id.0, "usr_", "userId")?;
    validate_access(&request.repository_access)?;
    validate_instant(&request.submitted_at)?;
    request
        .reserved_tokens
        .map(|value| validate_sql_range(value, "reservedTokens"))
        .transpose()?;
    request
        .reserved_cost_microunits
        .map(|value| validate_sql_range(value, "reservedCostMicrounits"))
        .transpose()?;
    if request.runtime_limit_millis == Some(0) {
        return Err(ExecutionAdmissionError::invalid(
            "runtimeLimitMillis must be positive",
        ));
    }
    request
        .runtime_limit_millis
        .map(|value| validate_sql_range(value, "runtimeLimitMillis"))
        .transpose()?;
    Ok(())
}

fn validate_start(request: &ExecutionReservationStart) -> Result<(), ExecutionAdmissionError> {
    validate_mutation_identity(
        &request.scope,
        &request.worker_pool_id,
        &request.job_id,
        &request.request_id,
        request.expected_revision,
        &request.started_at,
    )
}

fn validate_release(request: &ExecutionReservationRelease) -> Result<(), ExecutionAdmissionError> {
    validate_mutation_identity(
        &request.scope,
        &request.worker_pool_id,
        &request.job_id,
        &request.request_id,
        request.expected_revision,
        &request.released_at,
    )
}

fn validate_settlement(
    request: &ExecutionReservationSettlement,
) -> Result<(), ExecutionAdmissionError> {
    validate_mutation_identity(
        &request.scope,
        &request.worker_pool_id,
        &request.job_id,
        &request.request_id,
        request.expected_revision,
        &request.completed_at,
    )?;
    validate_sql_range(request.actual_tokens, "actualTokens")?;
    if let Some(cost) = request.actual_cost_microunits {
        validate_sql_range(cost, "actualCostMicrounits")?;
    }
    validate_sql_range(request.actual_runtime_millis, "actualRuntimeMillis")
}

fn validate_mutation_identity(
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
    job_id: &ExecutionJobId,
    request_id: &RequestId,
    expected_revision: u64,
    occurred_at: &Instant,
) -> Result<(), ExecutionAdmissionError> {
    validate_scope(scope)?;
    validate_id(&worker_pool_id.0, "wpl_", "workerPoolId")?;
    validate_id(&job_id.0, "job_", "jobId")?;
    validate_id(&request_id.0, "req_", "requestId")?;
    validate_instant(occurred_at)?;
    if expected_revision == 0 {
        return Err(ExecutionAdmissionError::invalid(
            "expectedRevision must be positive",
        ));
    }
    validate_sql_range(expected_revision, "expectedRevision")
}

fn validate_revision_state_time(
    current: &ExecutionReservationRecord,
    expected_revision: u64,
    expected_state: ExecutionReservationState,
    occurred_at: &Instant,
) -> Result<(), ExecutionAdmissionError> {
    validate_revision_time(current, expected_revision, occurred_at)?;
    if current.state != expected_state {
        return Err(ExecutionAdmissionError::invalid(
            "execution reservation state does not allow this operation",
        ));
    }
    Ok(())
}

fn validate_revision_time(
    current: &ExecutionReservationRecord,
    expected_revision: u64,
    occurred_at: &Instant,
) -> Result<(), ExecutionAdmissionError> {
    if current.revision != expected_revision {
        return Err(ExecutionAdmissionError::new(
            ExecutionAdmissionErrorCode::RevisionConflict,
            None,
            format!(
                "expected reservation revision {expected_revision}, but current revision is {}",
                current.revision
            ),
        ));
    }
    if occurred_at.0 < current.updated_at.0 {
        return Err(ExecutionAdmissionError::invalid(
            "execution reservation operation precedes durable state",
        ));
    }
    Ok(())
}

fn validate_policy(policy: &ExecutionAdmissionPolicy) -> Result<(), ExecutionAdmissionError> {
    validate_boundary(&policy.boundary)?;
    for (value, field) in [
        (Some(policy.limits.max_concurrent), "maxConcurrent"),
        (Some(policy.limits.max_queued), "maxQueued"),
        (policy.limits.token_budget, "tokenBudget"),
        (policy.limits.cost_budget_microunits, "costBudgetMicrounits"),
    ] {
        value
            .map(|value| validate_sql_range(value, field))
            .transpose()?;
    }
    if policy.limits.max_runtime_millis == Some(0) {
        return Err(ExecutionAdmissionError::invalid(
            "maxRuntimeMillis must be positive",
        ));
    }
    policy
        .limits
        .max_runtime_millis
        .map(|value| validate_sql_range(value, "maxRuntimeMillis"))
        .transpose()?;
    Ok(())
}

fn validate_boundary(boundary: &ExecutionAdmissionBoundary) -> Result<(), ExecutionAdmissionError> {
    match boundary {
        ExecutionAdmissionBoundary::Organization { organization_id } => {
            validate_id(&organization_id.0, "org_", "organizationId")
        }
        ExecutionAdmissionBoundary::Project {
            organization_id,
            project_id,
        } => {
            validate_id(&organization_id.0, "org_", "organizationId")?;
            validate_id(&project_id.0, "prj_", "projectId")
        }
        ExecutionAdmissionBoundary::Repository {
            organization_id,
            project_id,
            repository_id,
        } => {
            validate_id(&organization_id.0, "org_", "organizationId")?;
            validate_id(&project_id.0, "prj_", "projectId")?;
            validate_id(&repository_id.0, "rep_", "repositoryId")
        }
        ExecutionAdmissionBoundary::Delivery {
            organization_id,
            delivery_id,
        } => {
            validate_id(&organization_id.0, "org_", "organizationId")?;
            validate_id(&delivery_id.0, "dlv_", "deliveryId")
        }
        ExecutionAdmissionBoundary::ProductSession {
            organization_id,
            project_id,
            product_session_id,
        } => {
            validate_id(&organization_id.0, "org_", "organizationId")?;
            validate_id(&project_id.0, "prj_", "projectId")?;
            validate_id(&product_session_id.0, "psn_", "productSessionId")
        }
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id,
            worker_pool_id,
        } => {
            validate_id(&organization_id.0, "org_", "organizationId")?;
            validate_id(&worker_pool_id.0, "wpl_", "workerPoolId")
        }
    }
}

fn validate_scope(scope: &ExecutionQueueScope) -> Result<(), ExecutionAdmissionError> {
    validate_id(&scope.organization_id.0, "org_", "organizationId")?;
    validate_id(&scope.workspace_id.0, "wsp_", "workspaceId")?;
    validate_id(&scope.project_id.0, "prj_", "projectId")?;
    validate_id(&scope.repository_id.0, "rep_", "repositoryId")?;
    validate_id(&scope.product_session_id.0, "psn_", "productSessionId")?;
    if let Some(delivery_id) = &scope.delivery_id {
        validate_id(&delivery_id.0, "dlv_", "deliveryId")?;
    }
    Ok(())
}

fn validate_access(access: &ExecutionRepositoryAccess) -> Result<(), ExecutionAdmissionError> {
    if let ExecutionRepositoryAccess::IsolatedWrite { worktree_key } = access {
        let valid = !worktree_key.is_empty()
            && worktree_key.len() <= 200
            && worktree_key.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
            });
        if !valid {
            return Err(ExecutionAdmissionError::invalid(
                "worktreeKey is not portable",
            ));
        }
    }
    Ok(())
}

fn validate_id(value: &str, prefix: &str, field: &str) -> Result<(), ExecutionAdmissionError> {
    let valid = value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 26
            && suffix.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || matches!(
                        byte,
                        b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
                    )
            })
    });
    if valid {
        Ok(())
    } else {
        Err(ExecutionAdmissionError::invalid(format!(
            "{field} is not canonical"
        )))
    }
}

fn validate_instant(value: &Instant) -> Result<(), ExecutionAdmissionError> {
    let bytes = value.0.as_bytes();
    let valid = bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        });
    if valid {
        Ok(())
    } else {
        Err(ExecutionAdmissionError::invalid(
            "execution admission time is not canonical",
        ))
    }
}

fn access_columns(access: &ExecutionRepositoryAccess) -> (&'static str, Option<&str>) {
    match access {
        ExecutionRepositoryAccess::ReadOnly => ("read_only", None),
        ExecutionRepositoryAccess::SharedWrite => ("shared_write", None),
        ExecutionRepositoryAccess::IsolatedWrite { worktree_key } => {
            ("isolated_write", Some(worktree_key))
        }
    }
}

fn reservation_scope_key(
    scope: &ExecutionQueueScope,
    worker_pool_id: &WorkerPoolId,
) -> Result<String, ExecutionAdmissionError> {
    digest(&(scope, worker_pool_id))
}

fn boundary_key(boundary: &ExecutionAdmissionBoundary) -> Result<String, ExecutionAdmissionError> {
    digest(boundary)
}

fn digest<T: Serialize>(value: &T) -> Result<String, ExecutionAdmissionError> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        ExecutionAdmissionError::adapter(format!("failed to encode admission value: {error}"))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn encode_json<T: Serialize>(value: &T) -> Result<String, ExecutionAdmissionError> {
    serde_json::to_string(value).map_err(|error| {
        ExecutionAdmissionError::adapter(format!("failed to encode admission value: {error}"))
    })
}

fn decode_json<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, ExecutionAdmissionError> {
    serde_json::from_str(value).map_err(|error| {
        ExecutionAdmissionError::adapter(format!(
            "stored execution admission value is corrupt: {error}"
        ))
    })
}

fn validate_sql_range(value: u64, field: &str) -> Result<(), ExecutionAdmissionError> {
    if i64::try_from(value).is_ok() {
        Ok(())
    } else {
        Err(ExecutionAdmissionError::invalid(format!(
            "{field} exceeds the supported range"
        )))
    }
}

fn to_sql_integer(value: u64, field: &str) -> Result<i64, ExecutionAdmissionError> {
    i64::try_from(value).map_err(|_| {
        ExecutionAdmissionError::invalid(format!("{field} exceeds the supported range"))
    })
}

fn from_sql_integer(value: i64, field: &str) -> Result<u64, ExecutionAdmissionError> {
    u64::try_from(value).map_err(|_| {
        ExecutionAdmissionError::adapter(format!("stored {field} is outside the supported range"))
    })
}

#[allow(clippy::needless_pass_by_value)]
fn sql_error(error: rusqlite::Error) -> ExecutionAdmissionError {
    ExecutionAdmissionError::adapter(format!("execution admission SQLite failure: {error}"))
}

#[allow(clippy::needless_pass_by_value)]
fn storage_error(error: StorageError) -> ExecutionAdmissionError {
    ExecutionAdmissionError::adapter(format!("execution admission storage failure: {error}"))
}

#[cfg(test)]
mod limits_schema_tests {
    use super::*;

    #[test]
    fn limits_migration_preserves_rows_foreign_keys_and_rolls_back_corruption() {
        for (runtime_required, limits_required) in [(false, false), (false, true), (true, true)] {
            assert_limits_migration(runtime_required, limits_required);
        }
    }

    fn assert_limits_migration(runtime_required: bool, limits_required: bool) {
        let mut connection = Connection::open_in_memory().expect("database");
        let old_schema = legacy_admission_schema(runtime_required, limits_required);
        connection.execute_batch(&old_schema).expect("old schema");
        connection.execute_batch("INSERT INTO execution_admission_policies VALUES ('boundary', '{}', 1, 1, 1, 1, 1000);
            INSERT INTO execution_admission_reservations (job_id, scope_key, organization_id, workspace_id, project_id, repository_id, product_session_id, user_id, worker_pool_id, access_kind, state, reserved_tokens, reserved_cost_microunits, runtime_limit_millis, revision, submitted_at, updated_at) VALUES ('job', 'scope', 'org', 'workspace', 'project', 'repository', 'session', 'user', 'pool', 'read_only', 'queued', 1, 1, 1000, 1, 'time', 'time');
            INSERT INTO execution_admission_reservation_boundaries VALUES ('job', 'boundary', 0);
            CREATE TABLE migration_external_job (job TEXT REFERENCES execution_admission_reservations(job_id));
            INSERT INTO migration_external_job VALUES ('job');
            CREATE TABLE migration_external_child (boundary TEXT REFERENCES execution_admission_policies(boundary_key));
            INSERT INTO migration_external_child VALUES ('boundary');
            INSERT INTO execution_admission_receipts VALUES ('scope', 'request', 'digest', '{}');").expect("existing rows");
        prepare_admission_schema(&mut connection).expect("migrate");
        let schema: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'execution_admission_reservations'",
                [],
                |row| row.get(0),
            )
            .expect("schema");
        assert!(!schema.contains("actual_cost_microunits IS NOT NULL"));
        prepare_admission_schema(&mut connection).expect("reopen");
        let limit: i64 = connection
            .query_row(
                "SELECT max_runtime_millis FROM execution_admission_policies",
                [],
                |row| row.get(0),
            )
            .expect("preserved limit");
        assert_eq!(limit, 1000);
        for table in [
            "migration_external_child",
            "migration_external_job",
            "execution_admission_reservations",
            "execution_admission_reservation_boundaries",
            "execution_admission_receipts",
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("preserved row");
            assert_eq!(count, 1);
        }
        connection
            .execute(
                "UPDATE execution_admission_policies SET max_runtime_millis = NULL, token_budget = NULL, cost_budget_microunits = NULL",
                [],
            )
            .expect("explicit unlimited");
        connection.execute("UPDATE execution_admission_reservations SET reserved_tokens = NULL, reserved_cost_microunits = NULL", []).expect("unlimited reservation");
        assert!(
            connection
                .execute("DELETE FROM execution_admission_policies", [])
                .is_err()
        );

        assert_corrupt_schema_migration_rolls_back(&old_schema, limits_required);
    }

    fn legacy_admission_schema(runtime_required: bool, limits_required: bool) -> String {
        let schema = ADMISSION_SCHEMA
            .replace(
                "token_budget INTEGER CHECK",
                "token_budget INTEGER NOT NULL CHECK",
            )
            .replace(
                "cost_budget_microunits INTEGER CHECK",
                "cost_budget_microunits INTEGER NOT NULL CHECK",
            )
            .replace(
                "reserved_tokens INTEGER CHECK",
                "reserved_tokens INTEGER NOT NULL CHECK",
            )
            .replace(
                "reserved_cost_microunits INTEGER CHECK",
                "reserved_cost_microunits INTEGER NOT NULL CHECK",
            )
            .replace(
                "max_runtime_millis INTEGER CHECK",
                "max_runtime_millis INTEGER NOT NULL CHECK",
            )
            .replace(
                "runtime_limit_millis INTEGER CHECK",
                "runtime_limit_millis INTEGER NOT NULL CHECK",
            );
        let schema = if runtime_required {
            schema
        } else {
            schema
                .replace(
                    "max_runtime_millis INTEGER NOT NULL CHECK",
                    "max_runtime_millis INTEGER CHECK",
                )
                .replace(
                    "runtime_limit_millis INTEGER NOT NULL CHECK",
                    "runtime_limit_millis INTEGER CHECK",
                )
        };
        let schema = if limits_required {
            schema
        } else {
            ADMISSION_SCHEMA.to_owned()
        };
        schema.replace(
            "AND actual_runtime_millis IS NOT NULL\n            AND terminal_at",
            "AND actual_cost_microunits IS NOT NULL AND actual_runtime_millis IS NOT NULL\n            AND terminal_at",
        )
    }

    fn assert_corrupt_schema_migration_rolls_back(old_schema: &str, limits_required: bool) {
        let mut broken = Connection::open_in_memory().expect("broken database");
        broken
            .pragma_update(None, "foreign_keys", false)
            .expect("seed corruption");
        broken.execute_batch(old_schema).expect("old schema");
        broken.execute_batch("INSERT INTO execution_admission_reservation_boundaries VALUES ('missing-job', 'missing-policy', 0);").expect("preexisting corruption");
        assert!(prepare_admission_schema(&mut broken).is_err());
        let required: i64 = broken.query_row("SELECT \"notnull\" FROM pragma_table_info('execution_admission_policies') WHERE name = 'token_budget'", [], |row| row.get(0)).expect("rolled back schema");
        assert_eq!(required, i64::from(limits_required));
        let restored: String = broken
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'execution_admission_reservations'",
                [],
                |row| row.get(0),
            )
            .expect("rolled back cost constraint");
        assert!(restored.contains("actual_cost_microunits IS NOT NULL"));
        let foreign_keys: bool = broken
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("foreign keys");
        assert!(foreign_keys);
    }
}
