// SPDX-License-Identifier: Apache-2.0

//! Durable scheduler queue and its closed execution-job state machine.
//!
//! This module owns persistence and admission invariants only. Fair selection,
//! priority aging, retry policy, and resource reservations remain scheduler
//! concerns layered on top of the records exposed here.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    DeliveryId, ExecutionJobId, Instant, OrganizationId, ProductSessionId, ProjectId, RepositoryId,
    RequestId, Sha256Digest, StageRunId, WorkspaceId,
};

use crate::{SqliteStorage, StorageError, sql_error};

const EXECUTION_QUEUE_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS scheduler_execution_jobs (
    job_id TEXT PRIMARY KEY NOT NULL,
    organization_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    product_session_id TEXT NOT NULL,
    delivery_id TEXT,
    stage_run_id TEXT,
    submission_request_id TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    dispatch_payload BLOB NOT NULL,
    state TEXT NOT NULL CHECK (
        state IN ('queued', 'leased', 'running', 'cancelling', 'completed', 'failed')
    ),
    attempt INTEGER NOT NULL CHECK (attempt >= 1 AND attempt <= 1000),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    submitted_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    cancellation_request_id TEXT,
    cancellation_requested_at TEXT,
    CHECK (
        (cancellation_request_id IS NULL AND cancellation_requested_at IS NULL)
        OR
        (cancellation_request_id IS NOT NULL AND cancellation_requested_at IS NOT NULL)
    )
);
CREATE TABLE IF NOT EXISTS scheduler_execution_job_dependencies (
    job_id TEXT NOT NULL,
    dependency_job_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    PRIMARY KEY (job_id, dependency_job_id),
    UNIQUE (job_id, ordinal),
    FOREIGN KEY (job_id) REFERENCES scheduler_execution_jobs(job_id) ON DELETE CASCADE,
    FOREIGN KEY (dependency_job_id) REFERENCES scheduler_execution_jobs(job_id)
);
CREATE TABLE IF NOT EXISTS scheduler_execution_job_receipts (
    scope_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    operation TEXT NOT NULL,
    job_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (scope_key, request_id)
);
CREATE INDEX IF NOT EXISTS scheduler_execution_jobs_scoped_queue
    ON scheduler_execution_jobs (
        organization_id, workspace_id, project_id, repository_id,
        product_session_id, delivery_id, state, submitted_at, job_id
    );
CREATE UNIQUE INDEX IF NOT EXISTS scheduler_execution_jobs_active_stage_run
    ON scheduler_execution_jobs (stage_run_id)
    WHERE stage_run_id IS NOT NULL
      AND state IN ('queued', 'leased', 'running', 'cancelling');
CREATE INDEX IF NOT EXISTS scheduler_execution_job_dependencies_dependency
    ON scheduler_execution_job_dependencies (dependency_job_id, job_id);
";

const MAX_ATTEMPT: u64 = 1_000;
const MAX_DEPENDENCIES: usize = 1_024;
const MAX_DISPATCH_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_SIZE: usize = 100;

/// Exact product scope used for queue isolation and scheduler fairness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionQueueScope {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
    pub product_session_id: ProductSessionId,
    pub delivery_id: Option<DeliveryId>,
}

/// Closed durable lifecycle of one scheduler-owned job.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionJobState {
    Queued,
    Leased,
    Running,
    Cancelling,
    Completed,
    Failed,
}

impl ExecutionJobState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn from_stored(value: &str) -> Result<Self, StorageError> {
        match value {
            "queued" => Ok(Self::Queued),
            "leased" => Ok(Self::Leased),
            "running" => Ok(Self::Running),
            "cancelling" => Ok(Self::Cancelling),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            _ => Err(StorageError::adapter(
                "stored execution job state is invalid",
            )),
        }
    }
}

/// Durable cancellation intent retained through the terminal transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionJobCancellationIntent {
    pub request_id: RequestId,
    pub requested_at: Instant,
}

/// Input for creating one immutable queue entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionJobSubmission {
    pub scope: ExecutionQueueScope,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub payload_digest: Sha256Digest,
    /// Canonical serialized `ExecutionJob` bytes dispatched after admission.
    pub dispatch_payload: Vec<u8>,
    pub attempt: u64,
    pub dependencies: Vec<ExecutionJobId>,
    /// Delivery stage reservation. `ProductSession` Chat jobs have no `StageRun`.
    pub stage_run_id: Option<StageRunId>,
    pub submitted_at: Instant,
}

/// Optimistic, replay-safe request for one legal state transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionJobTransitionRequest {
    pub scope: ExecutionQueueScope,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub from: ExecutionJobState,
    pub to: ExecutionJobState,
    pub occurred_at: Instant,
}

/// Replay-safe cancellation command for one queued or active job.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionJobCancellationRequest {
    pub scope: ExecutionQueueScope,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub requested_at: Instant,
}

/// One complete durable queue record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionJobRecord {
    pub scope: ExecutionQueueScope,
    pub job_id: ExecutionJobId,
    pub submission_request_id: RequestId,
    pub payload_digest: Sha256Digest,
    pub dispatch_payload: Vec<u8>,
    pub state: ExecutionJobState,
    pub attempt: u64,
    pub revision: u64,
    pub dependencies: Vec<ExecutionJobId>,
    pub stage_run_id: Option<StageRunId>,
    pub submitted_at: Instant,
    pub updated_at: Instant,
    pub cancellation: Option<ExecutionJobCancellationIntent>,
}

/// Stored response for a submission, transition, or cancellation request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionJobMutationReceipt {
    pub job: ExecutionJobRecord,
    pub replayed: bool,
}

/// Stable cursor for an exact scope-local queue page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionJobPageCursor {
    pub submitted_at: Instant,
    pub job_id: ExecutionJobId,
}

/// One scope-isolated page ordered by submission time and job identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionJobPage {
    pub jobs: Vec<ExecutionJobRecord>,
    pub next_cursor: Option<ExecutionJobPageCursor>,
}

/// SQLite-backed durable execution queue.
pub struct ExecutionQueue<'storage> {
    storage: &'storage mut SqliteStorage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutionJobSubmissionMode {
    SubmitOrReplay,
    RequireNew,
    RequireReplay,
}

impl SqliteStorage {
    /// Opens the durable scheduler queue over this storage connection.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the queue schema cannot be prepared.
    pub fn execution_queue(&mut self) -> Result<ExecutionQueue<'_>, StorageError> {
        ExecutionQueue::new(self)
    }
}

impl<'storage> ExecutionQueue<'storage> {
    /// Opens the queue and creates its tables idempotently.
    ///
    /// # Errors
    ///
    /// Returns an adapter error when the queue schema cannot be prepared.
    pub fn new(storage: &'storage mut SqliteStorage) -> Result<Self, StorageError> {
        let queue = Self { storage };
        queue.ensure_schema()?;
        Ok(queue)
    }

    /// Submits exactly one durable job. An exact retry replays its stored
    /// receipt; a changed body using the same scoped request id is rejected.
    ///
    /// # Errors
    ///
    /// Rejects malformed input, missing/cross-scope dependencies, duplicate
    /// job identities, request conflicts, and `SQLite` failures.
    pub fn submit(
        &mut self,
        request: &ExecutionJobSubmission,
    ) -> Result<ExecutionJobMutationReceipt, StorageError> {
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let receipt = submit_execution_job_in_transaction(
            &transaction,
            request,
            ExecutionJobSubmissionMode::SubmitOrReplay,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    /// Applies one legal state transition with optimistic revision checking.
    /// Exact request retries replay the originally committed record even when
    /// later transitions have already advanced the job.
    ///
    /// # Errors
    ///
    /// Rejects malformed input, stale revisions, illegal transitions, blocked
    /// lease admission, request conflicts, and `SQLite` failures.
    pub fn transition(
        &mut self,
        request: &ExecutionJobTransitionRequest,
    ) -> Result<ExecutionJobMutationReceipt, StorageError> {
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let receipt = transition_execution_job_in_transaction(&transaction, request)?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    /// Persists a cancellation intent and immediately removes a queued or
    /// active job from lease admission by moving it to `cancelling`.
    ///
    /// # Errors
    ///
    /// Rejects malformed input, stale revisions, terminal/already-cancelling
    /// jobs, request conflicts, and `SQLite` failures.
    pub fn request_cancellation(
        &mut self,
        request: &ExecutionJobCancellationRequest,
    ) -> Result<ExecutionJobMutationReceipt, StorageError> {
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let receipt = cancel_execution_job_in_transaction(&transaction, request)?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    /// Loads one job only when every supplied scope component matches.
    ///
    /// # Errors
    ///
    /// Rejects malformed identities and `SQLite` failures.
    pub fn load_job(
        &self,
        scope: &ExecutionQueueScope,
        job_id: &ExecutionJobId,
    ) -> Result<Option<ExecutionJobRecord>, StorageError> {
        validate_scope(scope)?;
        validate_id(&job_id.0, "job_", "jobId")?;
        load_scoped_job(self.storage.connection()?, scope, job_id)
    }

    /// Returns one exact-scope page ordered deterministically by
    /// `(submitted_at, job_id)`. An empty state list includes all states.
    ///
    /// # Errors
    ///
    /// Rejects malformed scopes/cursors, duplicate state filters, invalid page
    /// sizes, and `SQLite` failures.
    pub fn list_jobs(
        &self,
        scope: &ExecutionQueueScope,
        states: &[ExecutionJobState],
        after: Option<&ExecutionJobPageCursor>,
        limit: usize,
    ) -> Result<ExecutionJobPage, StorageError> {
        validate_scope(scope)?;
        if limit == 0 || limit > MAX_PAGE_SIZE {
            return Err(StorageError::invalid_input(
                "execution job page size is outside the supported range",
            ));
        }
        let mut unique_states = HashSet::new();
        if states.iter().any(|state| !unique_states.insert(*state)) {
            return Err(StorageError::invalid_input(
                "execution job state filter contains duplicates",
            ));
        }
        if let Some(cursor) = after {
            validate_instant(&cursor.submitted_at, "cursor.submittedAt")?;
            validate_id(&cursor.job_id.0, "job_", "cursor.jobId")?;
        }

        let after_at = after.map_or("", |cursor| cursor.submitted_at.0.as_str());
        let after_job = after.map_or("", |cursor| cursor.job_id.0.as_str());
        let includes = |state| i64::from(states.is_empty() || states.contains(&state));
        let row_limit = i64::try_from(limit + 1)
            .map_err(|_| StorageError::invalid_input("execution job page size is invalid"))?;
        let connection = self.storage.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT job_id, organization_id, workspace_id, project_id, repository_id,
                        product_session_id, delivery_id, stage_run_id, submission_request_id,
                        payload_digest, dispatch_payload, state, attempt, revision, submitted_at,
                        updated_at, cancellation_request_id, cancellation_requested_at
                 FROM scheduler_execution_jobs
                 WHERE organization_id = ?1 AND workspace_id = ?2 AND project_id = ?3
                   AND repository_id = ?4 AND product_session_id = ?5 AND delivery_id IS ?6
                   AND (submitted_at > ?7 OR (submitted_at = ?7 AND job_id > ?8))
                   AND ((?9 = 1 AND state = 'queued')
                     OR (?10 = 1 AND state = 'leased')
                     OR (?11 = 1 AND state = 'running')
                     OR (?12 = 1 AND state = 'cancelling')
                     OR (?13 = 1 AND state = 'completed')
                     OR (?14 = 1 AND state = 'failed'))
                 ORDER BY submitted_at, job_id
                 LIMIT ?15",
            )
            .map_err(sql_error)?;
        let rows = statement
            .query_map(
                params![
                    scope.organization_id.0,
                    scope.workspace_id.0,
                    scope.project_id.0,
                    scope.repository_id.0,
                    scope.product_session_id.0,
                    scope.delivery_id.as_ref().map(|value| value.0.as_str()),
                    after_at,
                    after_job,
                    includes(ExecutionJobState::Queued),
                    includes(ExecutionJobState::Leased),
                    includes(ExecutionJobState::Running),
                    includes(ExecutionJobState::Cancelling),
                    includes(ExecutionJobState::Completed),
                    includes(ExecutionJobState::Failed),
                    row_limit,
                ],
                stored_job_from_row,
            )
            .map_err(sql_error)?;
        let stored = rows
            .map(|row| row.map_err(sql_error))
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut jobs = stored
            .into_iter()
            .map(|row| complete_record(connection, row))
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = jobs.len() > limit;
        if has_more {
            jobs.pop();
        }
        let next_cursor =
            has_more
                .then(|| jobs.last())
                .flatten()
                .map(|job| ExecutionJobPageCursor {
                    submitted_at: job.submitted_at.clone(),
                    job_id: job.job_id.clone(),
                });
        Ok(ExecutionJobPage { jobs, next_cursor })
    }

    /// Reports whether a scoped mutation receipt is durable. This does not
    /// authorize replay without the original request body.
    ///
    /// # Errors
    ///
    /// Rejects malformed input and `SQLite` failures.
    pub fn has_request(
        &self,
        scope: &ExecutionQueueScope,
        request_id: &RequestId,
    ) -> Result<bool, StorageError> {
        validate_scope(scope)?;
        validate_id(&request_id.0, "req_", "requestId")?;
        let scope_key = scope_key(scope)?;
        Ok(self
            .storage
            .connection()?
            .query_row(
                "SELECT 1 FROM scheduler_execution_job_receipts
                 WHERE scope_key = ?1 AND request_id = ?2",
                params![scope_key, request_id.0],
                |_| Ok(()),
            )
            .optional()
            .map_err(sql_error)?
            .is_some())
    }

    fn ensure_schema(&self) -> Result<(), StorageError> {
        ensure_execution_queue_schema(self.storage.connection()?)
    }
}

pub(crate) fn ensure_execution_queue_schema(connection: &Connection) -> Result<(), StorageError> {
    let table_exists = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'scheduler_execution_jobs'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some();
    if table_exists {
        let has_stage_run_id = {
            let mut statement = connection
                .prepare("PRAGMA table_info(scheduler_execution_jobs)")
                .map_err(sql_error)?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(sql_error)?;
            let mut found = false;
            for column in columns {
                if column.map_err(sql_error)? == "stage_run_id" {
                    found = true;
                    break;
                }
            }
            found
        };
        if !has_stage_run_id {
            connection
                .execute(
                    "ALTER TABLE scheduler_execution_jobs ADD COLUMN stage_run_id TEXT",
                    [],
                )
                .map_err(sql_error)?;
        }
    }
    connection
        .execute_batch(EXECUTION_QUEUE_SCHEMA)
        .map_err(sql_error)
}

pub(crate) fn transition_execution_job_in_transaction(
    connection: &Connection,
    request: &ExecutionJobTransitionRequest,
) -> Result<ExecutionJobMutationReceipt, StorageError> {
    validate_transition_request(request)?;
    let request_digest = digest(request)?;
    let scope_key = scope_key(&request.scope)?;
    if let Some(receipt) =
        replay_receipt(connection, &scope_key, &request.request_id, &request_digest)?
    {
        return Ok(receipt);
    }

    let current = load_scoped_job(connection, &request.scope, &request.job_id)?
        .ok_or_else(|| StorageError::invalid_input("execution job does not exist"))?;
    if current.revision != request.expected_revision {
        return Err(StorageError::revision_conflict(
            request.expected_revision,
            current.revision,
        ));
    }
    if current.state != request.from {
        return Err(StorageError::invalid_input(
            "execution job source state does not match durable state",
        ));
    }
    if !legal_transition(request.from, request.to) {
        return Err(StorageError::invalid_input(
            "execution job state transition is illegal",
        ));
    }
    if request.occurred_at.0 < current.updated_at.0 {
        return Err(StorageError::invalid_input(
            "execution job transition precedes its durable state",
        ));
    }
    if request.to == ExecutionJobState::Leased
        && has_blocking_dependency(connection, &request.job_id)?
    {
        return Err(StorageError::invalid_input(
            "execution job dependencies are not completed successfully",
        ));
    }

    let changed = connection
        .execute(
            "UPDATE scheduler_execution_jobs
             SET state = ?1, revision = revision + 1, updated_at = ?2
             WHERE job_id = ?3 AND revision = ?4 AND state = ?5",
            params![
                request.to.as_str(),
                request.occurred_at.0,
                request.job_id.0,
                i64::try_from(request.expected_revision).map_err(|_| {
                    StorageError::invalid_input("expected revision is out of range")
                })?,
                request.from.as_str(),
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StorageError::adapter(
            "execution job transition lost its transaction authority",
        ));
    }

    let job = load_scoped_job(connection, &request.scope, &request.job_id)?
        .ok_or_else(|| StorageError::adapter("transitioned execution job was not stored"))?;
    let receipt = ExecutionJobMutationReceipt {
        job,
        replayed: false,
    };
    insert_receipt(
        connection,
        &scope_key,
        &request.request_id,
        "transition",
        &request.job_id,
        &request_digest,
        &receipt,
    )?;
    Ok(receipt)
}

pub(crate) fn replace_execution_job_attempt_in_transaction(
    connection: &Connection,
    current: &ExecutionJobRecord,
    request_id: &RequestId,
    next_attempt: u64,
    dispatch_payload: &[u8],
    occurred_at: &Instant,
) -> Result<ExecutionJobMutationReceipt, StorageError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ReplacementDigest<'request> {
        job_id: &'request ExecutionJobId,
        request_id: &'request RequestId,
        expected_revision: u64,
        previous_attempt: u64,
        next_attempt: u64,
        dispatch_payload: &'request [u8],
        occurred_at: &'request Instant,
    }

    validate_scope(&current.scope)?;
    validate_id(&current.job_id.0, "job_", "jobId")?;
    validate_id(&request_id.0, "req_", "requestId")?;
    validate_instant(occurred_at, "occurredAt")?;
    if !matches!(
        current.state,
        ExecutionJobState::Leased | ExecutionJobState::Running | ExecutionJobState::Failed
    ) || current.cancellation.is_some()
        || next_attempt != current.attempt.saturating_add(1)
        || next_attempt > MAX_ATTEMPT
        || dispatch_payload.is_empty()
        || dispatch_payload.len() > MAX_DISPATCH_PAYLOAD_BYTES
        || occurred_at.0 < current.updated_at.0
    {
        return Err(StorageError::invalid_input(
            "execution job replacement authority is invalid",
        ));
    }
    let request = ReplacementDigest {
        job_id: &current.job_id,
        request_id,
        expected_revision: current.revision,
        previous_attempt: current.attempt,
        next_attempt,
        dispatch_payload,
        occurred_at,
    };
    let request_digest = digest(&request)?;
    let scope_key = scope_key(&current.scope)?;
    if let Some(receipt) = replay_receipt(connection, &scope_key, request_id, &request_digest)? {
        return Ok(receipt);
    }
    let durable = load_scoped_job(connection, &current.scope, &current.job_id)?
        .ok_or_else(|| StorageError::invalid_input("execution job does not exist"))?;
    if durable != *current {
        return Err(StorageError::revision_conflict(
            current.revision,
            durable.revision,
        ));
    }
    let changed = connection
        .execute(
            "UPDATE scheduler_execution_jobs
             SET state = 'leased', attempt = ?1, dispatch_payload = ?2,
                 revision = revision + 1, updated_at = ?3
             WHERE job_id = ?4 AND revision = ?5 AND attempt = ?6
               AND state = ?7 AND cancellation_request_id IS NULL",
            params![
                i64::try_from(next_attempt)
                    .map_err(|_| StorageError::invalid_input("attempt is out of range"))?,
                dispatch_payload,
                occurred_at.0,
                current.job_id.0,
                i64::try_from(current.revision).map_err(|_| {
                    StorageError::invalid_input("expected revision is out of range")
                })?,
                i64::try_from(current.attempt)
                    .map_err(|_| StorageError::invalid_input("attempt is out of range"))?,
                current.state.as_str(),
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StorageError::adapter(
            "execution job replacement lost its transaction authority",
        ));
    }
    let job = load_scoped_job(connection, &current.scope, &current.job_id)?
        .ok_or_else(|| StorageError::adapter("replaced execution job was not stored"))?;
    let receipt = ExecutionJobMutationReceipt {
        job,
        replayed: false,
    };
    insert_receipt(
        connection,
        &scope_key,
        request_id,
        "transition",
        &current.job_id,
        &request_digest,
        &receipt,
    )?;
    Ok(receipt)
}

pub(crate) fn cancel_execution_job_in_transaction(
    connection: &Connection,
    request: &ExecutionJobCancellationRequest,
) -> Result<ExecutionJobMutationReceipt, StorageError> {
    validate_cancellation_request(request)?;
    let request_digest = digest(request)?;
    let scope_key = scope_key(&request.scope)?;
    if let Some(receipt) =
        replay_receipt(connection, &scope_key, &request.request_id, &request_digest)?
    {
        return Ok(receipt);
    }

    let current = load_scoped_job(connection, &request.scope, &request.job_id)?
        .ok_or_else(|| StorageError::invalid_input("execution job does not exist"))?;
    if current.revision != request.expected_revision {
        return Err(StorageError::revision_conflict(
            request.expected_revision,
            current.revision,
        ));
    }
    if !matches!(
        current.state,
        ExecutionJobState::Queued | ExecutionJobState::Leased | ExecutionJobState::Running
    ) {
        return Err(StorageError::invalid_input(
            "execution job cannot accept cancellation in its durable state",
        ));
    }
    if request.requested_at.0 < current.updated_at.0 {
        return Err(StorageError::invalid_input(
            "execution job cancellation precedes its durable state",
        ));
    }

    let changed = connection
        .execute(
            "UPDATE scheduler_execution_jobs
             SET state = 'cancelling', revision = revision + 1, updated_at = ?1,
                 cancellation_request_id = ?2, cancellation_requested_at = ?1
             WHERE job_id = ?3 AND revision = ?4 AND state = ?5",
            params![
                request.requested_at.0,
                request.request_id.0,
                request.job_id.0,
                i64::try_from(request.expected_revision).map_err(|_| {
                    StorageError::invalid_input("expected revision is out of range")
                })?,
                current.state.as_str(),
            ],
        )
        .map_err(sql_error)?;
    if changed != 1 {
        return Err(StorageError::adapter(
            "execution job cancellation lost its transaction authority",
        ));
    }

    let job = load_scoped_job(connection, &request.scope, &request.job_id)?
        .ok_or_else(|| StorageError::adapter("cancelled execution job was not stored"))?;
    let receipt = ExecutionJobMutationReceipt {
        job,
        replayed: false,
    };
    insert_receipt(
        connection,
        &scope_key,
        &request.request_id,
        "cancel",
        &request.job_id,
        &request_digest,
        &receipt,
    )?;
    Ok(receipt)
}

pub(crate) fn submit_execution_job_in_transaction(
    connection: &Connection,
    request: &ExecutionJobSubmission,
    mode: ExecutionJobSubmissionMode,
) -> Result<ExecutionJobMutationReceipt, StorageError> {
    connection
        .execute_batch(EXECUTION_QUEUE_SCHEMA)
        .map_err(sql_error)?;
    let request = normalized_submission(request)?;
    let request_digest = digest(&request)?;
    let scope_key = scope_key(&request.scope)?;
    if let Some(receipt) =
        replay_receipt(connection, &scope_key, &request.request_id, &request_digest)?
    {
        if mode == ExecutionJobSubmissionMode::RequireNew {
            return Err(StorageError::invalid_input(
                "execution job receipt exists without its atomic state receipt",
            ));
        }
        require_exact_submission_receipt(&request, &receipt)?;
        return Ok(receipt);
    }
    if mode == ExecutionJobSubmissionMode::RequireReplay {
        return Err(StorageError::request_replay_missing(&request.request_id));
    }
    insert_submission(connection, &request, &scope_key, &request_digest)
}

fn insert_submission(
    connection: &Connection,
    request: &ExecutionJobSubmission,
    scope_key: &str,
    request_digest: &str,
) -> Result<ExecutionJobMutationReceipt, StorageError> {
    if job_exists(connection, &request.job_id)? {
        return Err(StorageError::invalid_input(
            "execution job identity already exists",
        ));
    }
    for dependency in &request.dependencies {
        let Some(scope) = load_job_scope(connection, dependency)? else {
            return Err(StorageError::invalid_input(
                "execution job dependency does not exist",
            ));
        };
        if scope != request.scope {
            return Err(StorageError::invalid_input(
                "execution job dependency belongs to another scope",
            ));
        }
    }

    connection
        .execute(
            "INSERT INTO scheduler_execution_jobs
                (job_id, organization_id, workspace_id, project_id, repository_id,
                 product_session_id, delivery_id, stage_run_id, submission_request_id,
                 payload_digest, dispatch_payload, state, attempt, revision, submitted_at,
                 updated_at, cancellation_request_id, cancellation_requested_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'queued', ?12, 1,
                     ?13, ?13, NULL, NULL)",
            params![
                request.job_id.0,
                request.scope.organization_id.0,
                request.scope.workspace_id.0,
                request.scope.project_id.0,
                request.scope.repository_id.0,
                request.scope.product_session_id.0,
                request
                    .scope
                    .delivery_id
                    .as_ref()
                    .map(|value| value.0.as_str()),
                request.stage_run_id.as_ref().map(|value| value.0.as_str()),
                request.request_id.0,
                request.payload_digest.0,
                request.dispatch_payload,
                i64::try_from(request.attempt)
                    .map_err(|_| StorageError::invalid_input("attempt is out of range"))?,
                request.submitted_at.0,
            ],
        )
        .map_err(sql_error)?;
    for (ordinal, dependency) in request.dependencies.iter().enumerate() {
        connection
            .execute(
                "INSERT INTO scheduler_execution_job_dependencies
                    (job_id, dependency_job_id, ordinal)
                 VALUES (?1, ?2, ?3)",
                params![
                    request.job_id.0,
                    dependency.0,
                    i64::try_from(ordinal).map_err(|_| StorageError::invalid_input(
                        "dependency position is out of range"
                    ))?,
                ],
            )
            .map_err(sql_error)?;
    }

    let job = load_scoped_job(connection, &request.scope, &request.job_id)?
        .ok_or_else(|| StorageError::adapter("submitted execution job was not stored"))?;
    let receipt = ExecutionJobMutationReceipt {
        job,
        replayed: false,
    };
    insert_receipt(
        connection,
        scope_key,
        &request.request_id,
        "submit",
        &request.job_id,
        request_digest,
        &receipt,
    )?;
    Ok(receipt)
}

fn require_exact_submission_receipt(
    request: &ExecutionJobSubmission,
    receipt: &ExecutionJobMutationReceipt,
) -> Result<(), StorageError> {
    let job = &receipt.job;
    if job.scope != request.scope
        || job.job_id != request.job_id
        || job.submission_request_id != request.request_id
        || job.payload_digest != request.payload_digest
        || job.dispatch_payload != request.dispatch_payload
        || job.attempt != request.attempt
        || job.stage_run_id != request.stage_run_id
        || job.revision != 1
        || job.state != ExecutionJobState::Queued
        || job.dependencies != request.dependencies
        || job.submitted_at != request.submitted_at
        || job.updated_at != request.submitted_at
        || job.cancellation.is_some()
    {
        return Err(StorageError::adapter(
            "execution job submission receipt is inconsistent",
        ));
    }
    Ok(())
}

fn normalized_submission(
    request: &ExecutionJobSubmission,
) -> Result<ExecutionJobSubmission, StorageError> {
    validate_scope(&request.scope)?;
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_digest(&request.payload_digest)?;
    validate_instant(&request.submitted_at, "submittedAt")?;
    if request.dispatch_payload.is_empty()
        || request.dispatch_payload.len() > MAX_DISPATCH_PAYLOAD_BYTES
    {
        return Err(StorageError::invalid_input(
            "execution job dispatch payload size is invalid",
        ));
    }
    if request.attempt == 0 || request.attempt > MAX_ATTEMPT {
        return Err(StorageError::invalid_input(
            "execution job attempt is outside the supported range",
        ));
    }
    if let Some(stage_run_id) = &request.stage_run_id {
        validate_id(&stage_run_id.0, "run_", "stageRunId")?;
        if request.scope.delivery_id.is_none() {
            return Err(StorageError::invalid_input(
                "ProductSession execution job cannot reserve a StageRun",
            ));
        }
    } else if request.scope.delivery_id.is_some() {
        return Err(StorageError::invalid_input(
            "Delivery execution job must reserve its StageRun",
        ));
    }
    if request.dependencies.len() > MAX_DEPENDENCIES {
        return Err(StorageError::invalid_input(
            "execution job dependencies exceed the maximum count",
        ));
    }
    let mut normalized = request.clone();
    normalized
        .dependencies
        .sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for dependency in &normalized.dependencies {
        validate_id(&dependency.0, "job_", "dependencies.jobId")?;
        if dependency == &request.job_id {
            return Err(StorageError::invalid_input(
                "execution job cannot depend on itself",
            ));
        }
    }
    if normalized
        .dependencies
        .windows(2)
        .any(|pair| pair[0] == pair[1])
    {
        return Err(StorageError::invalid_input(
            "execution job dependencies contain duplicates",
        ));
    }
    Ok(normalized)
}

fn validate_transition_request(
    request: &ExecutionJobTransitionRequest,
) -> Result<(), StorageError> {
    validate_scope(&request.scope)?;
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_instant(&request.occurred_at, "occurredAt")?;
    if request.expected_revision == 0 || request.expected_revision > i64::MAX as u64 {
        return Err(StorageError::invalid_input(
            "expected revision is outside the supported range",
        ));
    }
    Ok(())
}

fn validate_cancellation_request(
    request: &ExecutionJobCancellationRequest,
) -> Result<(), StorageError> {
    validate_scope(&request.scope)?;
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_instant(&request.requested_at, "requestedAt")?;
    if request.expected_revision == 0 || request.expected_revision > i64::MAX as u64 {
        return Err(StorageError::invalid_input(
            "expected revision is outside the supported range",
        ));
    }
    Ok(())
}

fn validate_scope(scope: &ExecutionQueueScope) -> Result<(), StorageError> {
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

fn legal_transition(from: ExecutionJobState, to: ExecutionJobState) -> bool {
    matches!(
        (from, to),
        (
            ExecutionJobState::Queued,
            ExecutionJobState::Leased | ExecutionJobState::Failed
        ) | (
            ExecutionJobState::Leased,
            ExecutionJobState::Running | ExecutionJobState::Queued | ExecutionJobState::Failed
        ) | (
            ExecutionJobState::Running,
            ExecutionJobState::Queued | ExecutionJobState::Completed | ExecutionJobState::Failed
        ) | (
            ExecutionJobState::Cancelling,
            ExecutionJobState::Completed | ExecutionJobState::Failed
        )
    )
}

fn job_exists(connection: &Connection, job_id: &ExecutionJobId) -> Result<bool, StorageError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM scheduler_execution_jobs WHERE job_id = ?1",
            [&job_id.0],
            |_| Ok(()),
        )
        .optional()
        .map_err(sql_error)?
        .is_some())
}

fn has_blocking_dependency(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<bool, StorageError> {
    Ok(connection
        .query_row(
            "SELECT 1
             FROM scheduler_execution_job_dependencies d
             LEFT JOIN scheduler_execution_jobs dependency
               ON dependency.job_id = d.dependency_job_id
             WHERE d.job_id = ?1
               AND (dependency.job_id IS NULL OR dependency.state != 'completed'
                    OR dependency.cancellation_request_id IS NOT NULL)
             LIMIT 1",
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
) -> Result<Option<ExecutionJobMutationReceipt>, StorageError> {
    let Some((stored_digest, response_json)) = connection
        .query_row(
            "SELECT request_digest, response_json
             FROM scheduler_execution_job_receipts
             WHERE scope_key = ?1 AND request_id = ?2",
            params![scope_key, request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?
    else {
        return Ok(None);
    };
    if stored_digest != request_digest {
        return Err(StorageError::request_conflict(request_id));
    }
    let mut receipt: ExecutionJobMutationReceipt = decode_json(&response_json)?;
    validate_stored_record(&receipt.job)?;
    receipt.replayed = true;
    Ok(Some(receipt))
}

fn insert_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    operation: &str,
    job_id: &ExecutionJobId,
    request_digest: &str,
    receipt: &ExecutionJobMutationReceipt,
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO scheduler_execution_job_receipts
                (scope_key, request_id, operation, job_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                scope_key,
                request_id.0,
                operation,
                job_id.0,
                request_digest,
                encode_json(receipt)?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn load_job_scope(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionQueueScope>, StorageError> {
    connection
        .query_row(
            "SELECT organization_id, workspace_id, project_id, repository_id,
                    product_session_id, delivery_id
             FROM scheduler_execution_jobs WHERE job_id = ?1",
            [&job_id.0],
            |row| {
                Ok(ExecutionQueueScope {
                    organization_id: OrganizationId(row.get(0)?),
                    workspace_id: WorkspaceId(row.get(1)?),
                    project_id: ProjectId(row.get(2)?),
                    repository_id: RepositoryId(row.get(3)?),
                    product_session_id: ProductSessionId(row.get(4)?),
                    delivery_id: row.get::<_, Option<String>>(5)?.map(DeliveryId),
                })
            },
        )
        .optional()
        .map_err(sql_error)
        .and_then(|scope| {
            if let Some(scope) = &scope {
                validate_scope(scope)?;
            }
            Ok(scope)
        })
}

pub(crate) fn load_scoped_job(
    connection: &Connection,
    scope: &ExecutionQueueScope,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionJobRecord>, StorageError> {
    let stored = connection
        .query_row(
            "SELECT job_id, organization_id, workspace_id, project_id, repository_id,
                    product_session_id, delivery_id, stage_run_id, submission_request_id,
                    payload_digest, dispatch_payload, state, attempt, revision, submitted_at,
                    updated_at, cancellation_request_id, cancellation_requested_at
             FROM scheduler_execution_jobs
             WHERE job_id = ?1 AND organization_id = ?2 AND workspace_id = ?3
               AND project_id = ?4 AND repository_id = ?5 AND product_session_id = ?6
               AND delivery_id IS ?7",
            params![
                job_id.0,
                scope.organization_id.0,
                scope.workspace_id.0,
                scope.project_id.0,
                scope.repository_id.0,
                scope.product_session_id.0,
                scope.delivery_id.as_ref().map(|value| value.0.as_str()),
            ],
            stored_job_from_row,
        )
        .optional()
        .map_err(sql_error)?;
    stored
        .map(|row| complete_record(connection, row))
        .transpose()
}

pub(crate) struct StoredJobRow {
    job_id: String,
    organization_id: String,
    workspace_id: String,
    project_id: String,
    repository_id: String,
    product_session_id: String,
    delivery_id: Option<String>,
    stage_run_id: Option<String>,
    submission_request_id: String,
    payload_digest: String,
    dispatch_payload: Vec<u8>,
    state: String,
    attempt: i64,
    revision: i64,
    submitted_at: String,
    updated_at: String,
    cancellation_request_id: Option<String>,
    cancellation_requested_at: Option<String>,
}

pub(crate) fn stored_job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredJobRow> {
    Ok(StoredJobRow {
        job_id: row.get(0)?,
        organization_id: row.get(1)?,
        workspace_id: row.get(2)?,
        project_id: row.get(3)?,
        repository_id: row.get(4)?,
        product_session_id: row.get(5)?,
        delivery_id: row.get(6)?,
        stage_run_id: row.get(7)?,
        submission_request_id: row.get(8)?,
        payload_digest: row.get(9)?,
        dispatch_payload: row.get(10)?,
        state: row.get(11)?,
        attempt: row.get(12)?,
        revision: row.get(13)?,
        submitted_at: row.get(14)?,
        updated_at: row.get(15)?,
        cancellation_request_id: row.get(16)?,
        cancellation_requested_at: row.get(17)?,
    })
}

pub(crate) fn complete_record(
    connection: &Connection,
    stored: StoredJobRow,
) -> Result<ExecutionJobRecord, StorageError> {
    let mut statement = connection
        .prepare(
            "SELECT dependency_job_id
             FROM scheduler_execution_job_dependencies
             WHERE job_id = ?1 ORDER BY ordinal",
        )
        .map_err(sql_error)?;
    let dependencies = statement
        .query_map([&stored.job_id], |row| {
            row.get::<_, String>(0).map(ExecutionJobId)
        })
        .map_err(sql_error)?
        .map(|row| row.map_err(sql_error))
        .collect::<Result<Vec<_>, _>>()?;
    let attempt = u64::try_from(stored.attempt)
        .map_err(|_| StorageError::adapter("stored execution job attempt is invalid"))?;
    let revision = u64::try_from(stored.revision)
        .map_err(|_| StorageError::adapter("stored execution job revision is invalid"))?;
    let cancellation = match (
        stored.cancellation_request_id,
        stored.cancellation_requested_at,
    ) {
        (None, None) => None,
        (Some(request_id), Some(requested_at)) => Some(ExecutionJobCancellationIntent {
            request_id: RequestId(request_id),
            requested_at: Instant(requested_at),
        }),
        _ => {
            return Err(StorageError::adapter(
                "stored execution job cancellation intent is incomplete",
            ));
        }
    };
    let record = ExecutionJobRecord {
        scope: ExecutionQueueScope {
            organization_id: OrganizationId(stored.organization_id),
            workspace_id: WorkspaceId(stored.workspace_id),
            project_id: ProjectId(stored.project_id),
            repository_id: RepositoryId(stored.repository_id),
            product_session_id: ProductSessionId(stored.product_session_id),
            delivery_id: stored.delivery_id.map(DeliveryId),
        },
        job_id: ExecutionJobId(stored.job_id),
        submission_request_id: RequestId(stored.submission_request_id),
        payload_digest: Sha256Digest(stored.payload_digest),
        dispatch_payload: stored.dispatch_payload,
        state: ExecutionJobState::from_stored(&stored.state)?,
        attempt,
        revision,
        dependencies,
        stage_run_id: stored.stage_run_id.map(StageRunId),
        submitted_at: Instant(stored.submitted_at),
        updated_at: Instant(stored.updated_at),
        cancellation,
    };
    validate_stored_record(&record)?;
    Ok(record)
}

fn validate_stored_record(record: &ExecutionJobRecord) -> Result<(), StorageError> {
    validate_scope(&record.scope)
        .map_err(|_| StorageError::adapter("stored execution job scope is invalid"))?;
    validate_id(&record.job_id.0, "job_", "jobId")
        .map_err(|_| StorageError::adapter("stored execution job identity is invalid"))?;
    validate_id(
        &record.submission_request_id.0,
        "req_",
        "submissionRequestId",
    )
    .map_err(|_| StorageError::adapter("stored submission request identity is invalid"))?;
    validate_digest(&record.payload_digest)
        .map_err(|_| StorageError::adapter("stored execution payload digest is invalid"))?;
    if let Some(stage_run_id) = &record.stage_run_id {
        validate_id(&stage_run_id.0, "run_", "stageRunId")
            .map_err(|_| StorageError::adapter("stored StageRun identity is invalid"))?;
    }
    if record.scope.delivery_id.is_some() != record.stage_run_id.is_some() {
        return Err(StorageError::adapter(
            "stored execution StageRun reservation is inconsistent",
        ));
    }
    validate_instant(&record.submitted_at, "submittedAt")
        .map_err(|_| StorageError::adapter("stored submission time is invalid"))?;
    validate_instant(&record.updated_at, "updatedAt")
        .map_err(|_| StorageError::adapter("stored update time is invalid"))?;
    if record.updated_at.0 < record.submitted_at.0
        || record.dispatch_payload.is_empty()
        || record.dispatch_payload.len() > MAX_DISPATCH_PAYLOAD_BYTES
        || record.attempt == 0
        || record.attempt > MAX_ATTEMPT
        || record.revision == 0
        || record.dependencies.len() > MAX_DEPENDENCIES
    {
        return Err(StorageError::adapter(
            "stored execution job values are invalid",
        ));
    }
    for dependency in &record.dependencies {
        validate_id(&dependency.0, "job_", "dependencies.jobId")
            .map_err(|_| StorageError::adapter("stored dependency identity is invalid"))?;
    }
    if let Some(cancellation) = &record.cancellation {
        validate_id(&cancellation.request_id.0, "req_", "cancellation.requestId")
            .map_err(|_| StorageError::adapter("stored cancellation request is invalid"))?;
        validate_instant(&cancellation.requested_at, "cancellation.requestedAt")
            .map_err(|_| StorageError::adapter("stored cancellation time is invalid"))?;
    }
    if record.state == ExecutionJobState::Cancelling && record.cancellation.is_none() {
        return Err(StorageError::adapter(
            "stored cancelling job has no cancellation intent",
        ));
    }
    Ok(())
}

fn scope_key(scope: &ExecutionQueueScope) -> Result<String, StorageError> {
    digest(scope)
}

fn digest<T: Serialize>(value: &T) -> Result<String, StorageError> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        StorageError::adapter(format!("failed to encode execution queue request: {error}"))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn encode_json<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| {
        StorageError::adapter(format!("failed to encode execution queue receipt: {error}"))
    })
}

fn decode_json<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|error| {
        StorageError::adapter(format!(
            "stored execution queue receipt is corrupt: {error}"
        ))
    })
}

fn validate_id(value: &str, prefix: &str, field: &str) -> Result<(), StorageError> {
    let valid = value
        .strip_prefix(prefix)
        .is_some_and(|suffix| suffix.len() == 26 && suffix.bytes().all(crockford_byte));
    if valid {
        Ok(())
    } else {
        Err(StorageError::invalid_input(format!(
            "{field} is not canonical"
        )))
    }
}

fn crockford_byte(byte: u8) -> bool {
    byte.is_ascii_digit()
        || matches!(
            byte,
            b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z'
        )
}

fn validate_digest(digest: &Sha256Digest) -> Result<(), StorageError> {
    let Some(value) = digest.0.strip_prefix("sha256:") else {
        return Err(StorageError::invalid_input(
            "execution payload digest is not canonical",
        ));
    };
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(StorageError::invalid_input(
            "execution payload digest is not canonical",
        ));
    }
    Ok(())
}

fn validate_instant(value: &Instant, field: &str) -> Result<(), StorageError> {
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
        Err(StorageError::invalid_input(format!(
            "{field} is not canonical"
        )))
    }
}
