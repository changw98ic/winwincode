// SPDX-License-Identifier: Apache-2.0

//! Repository-scoped durable scheduler authority.
//!
//! One immediate `SQLite` transaction owns fair selection, the queue lease
//! transition, the Registry claim, and the scheduler response receipt. The
//! caller never lists private tables or constructs lease authority.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId, OrganizationId, ProjectId,
    RepositoryId, RequestId, WorkerId, WorkerInstanceId, WorkerSessionId, WorkspaceId,
};

use crate::execution_queue::{
    cancel_execution_job_in_transaction, complete_record, ensure_execution_queue_schema,
    replace_execution_job_attempt_in_transaction, stored_job_from_row,
    transition_execution_job_in_transaction,
};
use crate::execution_registry::{
    claim_execution_job_in_transaction, finish_execution_lease_in_transaction,
    load_dispatch_authority_in_transaction, load_lease_in_transaction,
    record_dispatch_result_in_transaction,
};
use crate::execution_scope_replacement::{
    EXECUTION_SCOPE_REPLACEMENT_SCHEMA, NewExecutionScopeReplacement,
    insert_execution_scope_replacement,
};
use crate::repository_scheduler_replacement::{
    logical_dispatch_digest, replacement_dispatch_payload,
};
use crate::scheduler_policy::{
    SchedulerRetryDecision, SchedulerRetryPolicy, scheduler_retry_decision,
};
use crate::worker_session_slots::{
    ensure_worker_session_slot_schema, fence_worker_session_for_replacement_in_transaction,
    load_slot_in_transaction,
};
use crate::{
    DispatchResultReceipt, DispatchResultRequest, ExecutionJobCancellationRequest,
    ExecutionJobRecord, ExecutionJobState, ExecutionJobTransitionRequest, ExecutionLeaseClaim,
    ExecutionLeaseRecord, ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest,
    LeaseWriteStatus, SqliteStorage, StorageError, WorkerSlotAuthority, sql_error,
};

const SCHEDULER_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS repository_scheduler_drive_receipts (
    scope_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (scope_key, request_id)
);
CREATE TABLE IF NOT EXISTS repository_scheduler_cancel_receipts (
    scope_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (scope_key, request_id)
);
CREATE TABLE IF NOT EXISTS repository_scheduler_retry_receipts (
    scope_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    response_json TEXT NOT NULL,
    PRIMARY KEY (scope_key, request_id)
);
CREATE TABLE IF NOT EXISTS repository_scheduler_offers (
    scope_key TEXT NOT NULL,
    scheduler_generation TEXT NOT NULL,
    job_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    offered_at TEXT NOT NULL,
    PRIMARY KEY (scope_key, scheduler_generation, job_id)
);
CREATE TABLE IF NOT EXISTS repository_scheduler_fairness (
    scope_key TEXT NOT NULL,
    product_session_id TEXT NOT NULL,
    delivery_key TEXT NOT NULL,
    last_sequence INTEGER NOT NULL CHECK (last_sequence > 0),
    PRIMARY KEY (scope_key, product_session_id, delivery_key)
);
CREATE TABLE IF NOT EXISTS repository_scheduler_state (
    scope_key TEXT PRIMARY KEY NOT NULL,
    next_sequence INTEGER NOT NULL CHECK (next_sequence > 0)
);
";

const MAX_REPOSITORY_JOBS: usize = 100_000;
const IDENTIFIER_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const LEASE_ID_NAMESPACE: &[u8] = b"winwincode.repository-scheduler.lease.v1";
const MESSAGE_ID_NAMESPACE: &[u8] = b"winwincode.repository-scheduler.dispatch-message.v1";
const CLAIM_ID_NAMESPACE: &[u8] = b"winwincode.repository-scheduler.registry-claim.v1";
const QUEUE_ID_NAMESPACE: &[u8] = b"winwincode.repository-scheduler.queue-lease.v1";
const CANCEL_MESSAGE_ID_NAMESPACE: &[u8] = b"winwincode.repository-scheduler.cancel-message.v1";
const CANCEL_QUEUE_TERMINAL_ID_NAMESPACE: &[u8] =
    b"winwincode.repository-scheduler.cancel-queue-terminal.v1";
const CANCEL_LEASE_TERMINAL_ID_NAMESPACE: &[u8] =
    b"winwincode.repository-scheduler.cancel-lease-terminal.v1";
const DISPATCH_QUEUE_ID_NAMESPACE: &[u8] = b"winwincode.repository-scheduler.dispatch-queue.v1";
const DISPATCH_LEASE_TERMINAL_ID_NAMESPACE: &[u8] =
    b"winwincode.repository-scheduler.dispatch-lease-terminal.v1";
const REPLACEMENT_LEASE_TERMINAL_ID_NAMESPACE: &[u8] =
    b"winwincode.repository-scheduler.replacement-lease-terminal.v1";

/// One configured repository without `ProductSession` or Delivery narrowing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositorySchedulerScope {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub repository_id: RepositoryId,
}

/// One receipt-bearing scheduler drive request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositorySchedulerClaimRequest {
    pub scope: RepositorySchedulerScope,
    pub request_id: RequestId,
    /// Process-unique Control Plane boot identity. A new generation may
    /// re-offer active work once; the same generation does not spin.
    pub scheduler_generation: String,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub issued_at: Instant,
    pub expires_at: Instant,
}

/// Exact dispatch authority returned only after queue and Registry commit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositorySchedulerClaimReceipt {
    pub job: ExecutionJobRecord,
    pub lease: ExecutionLeaseRecord,
    pub message_id: ExecutionMessageId,
    pub request_id: RequestId,
    pub replayed: bool,
    pub recovered: bool,
}

/// Scheduler-owned finite retry of one durable failed Job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySchedulerRetryRequest {
    pub scope: RepositorySchedulerScope,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub scheduler_generation: String,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub retryable_failure: bool,
    pub failed_at_tick: u64,
    pub now_tick: u64,
    pub policy: SchedulerRetryPolicy,
    pub issued_at: Instant,
    pub expires_at: Instant,
}

/// One Worker dispatch result joined to its repository queue revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySchedulerDispatchResultRequest {
    pub scope: RepositorySchedulerScope,
    pub dispatch: DispatchResultRequest,
}

/// Atomic Registry dispatch receipt and resulting queue record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySchedulerDispatchResultReceipt {
    pub job: ExecutionJobRecord,
    pub dispatch: DispatchResultReceipt,
    pub accepted: bool,
}

/// One repository-scoped cancellation request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositorySchedulerCancellationRequest {
    pub scope: RepositorySchedulerScope,
    pub job_id: ExecutionJobId,
    pub request_id: RequestId,
    pub expected_revision: u64,
    pub requested_at: Instant,
}

/// Durable cancellation result. `worker_session_id` and `message_id` are both
/// present only when the accepted dispatch must receive `job.cancel`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositorySchedulerCancellationReceipt {
    pub job: ExecutionJobRecord,
    pub lease: Option<ExecutionLeaseRecord>,
    pub worker_session_id: Option<WorkerSessionId>,
    pub message_id: Option<ExecutionMessageId>,
    pub request_id: RequestId,
    pub replayed: bool,
}

/// One exact terminal settlement joining queue state and Registry capacity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySchedulerTerminalRequest {
    pub scope: RepositorySchedulerScope,
    pub terminal: ExecutionLeaseTerminalRequest,
}

/// Durable result of atomically terminalizing the queue and Registry lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySchedulerTerminalReceipt {
    pub job: ExecutionJobRecord,
    pub lease_inserted: bool,
    pub replayed: bool,
}

enum DriveReceiptReplay {
    Missing,
    Stored(Box<Option<RepositorySchedulerClaimReceipt>>),
}

enum ActiveJobRecovery {
    Wait,
    Reoffer(ExecutionJobRecord),
    Replace {
        job: ExecutionJobRecord,
        lease: ExecutionLeaseRecord,
    },
}

/// Borrowed repository scheduler over the one canonical product database.
pub struct RepositoryScheduler<'storage> {
    storage: &'storage mut SqliteStorage,
}

impl SqliteStorage {
    /// Opens the repository scheduler over this storage connection.
    ///
    /// # Errors
    ///
    /// Returns a storage error when any canonical queue, Registry, or scheduler
    /// table cannot be prepared.
    pub fn repository_scheduler(&mut self) -> Result<RepositoryScheduler<'_>, StorageError> {
        RepositoryScheduler::new(self)
    }
}

impl<'storage> RepositoryScheduler<'storage> {
    /// Prepares all scheduler-owned tables on the same `SQLite` connection.
    ///
    /// # Errors
    ///
    /// Returns the first schema/storage failure.
    pub fn new(storage: &'storage mut SqliteStorage) -> Result<Self, StorageError> {
        ensure_execution_queue_schema(storage.connection()?)?;
        // Opening the canonical Registry is the only supported schema owner.
        {
            let _registry = storage.execution_registry()?;
        }
        ensure_worker_session_slot_schema(storage.connection()?).map_err(|error| {
            StorageError::adapter(format!("Worker slot schema failed: {error}"))
        })?;
        storage
            .connection()?
            .execute_batch(&format!(
                "{SCHEDULER_SCHEMA}{EXECUTION_SCOPE_REPLACEMENT_SCHEMA}"
            ))
            .map_err(sql_error)?;
        // FLOW-100.4: a job dispatched to a Device WorkerSession carries one
        // durable device execution fact row, and queue selection excludes such
        // jobs so the local embedded worker can never claim device-owned work.
        // The marker table is owned by the device execution binding ledger;
        // ensuring it here keeps the exclusion predicate working on every
        // database that selects the queue, whatever ledger was opened first.
        storage
            .connection()?
            .execute_batch(
                crate::device_execution_binding::DEVICE_EXECUTION_RESERVATION_FACTS_SCHEMA,
            )
            .map_err(sql_error)?;
        Ok(Self { storage })
    }

    /// Fairly claims or recovers one repository job.
    ///
    /// Exact request replay returns the original result before consulting live
    /// Worker capacity or current queue state. A changed body conflicts. One
    /// process generation receives each active job at most once, while a new
    /// boot generation re-offers the exact stored lease/message after restart.
    ///
    /// # Errors
    ///
    /// Rejects malformed scope/time/worker facts, a changed replay, corrupt
    /// active authority, a rejected Registry claim, and `SQLite` failures.
    pub fn claim_next(
        &mut self,
        request: &RepositorySchedulerClaimRequest,
    ) -> Result<Option<RepositorySchedulerClaimReceipt>, StorageError> {
        validate_claim_request(request)?;
        let scope_key = repository_scope_key(&request.scope);
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;

        if let DriveReceiptReplay::Stored(mut receipt) = replay_drive_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
        )? {
            if let Some(receipt) = receipt.as_mut() {
                receipt.replayed = true;
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(*receipt);
        }

        let selected = match recover_active_job(&transaction, request, &scope_key)? {
            Some(ActiveJobRecovery::Wait) => {
                transaction.commit().map_err(sql_error)?;
                return Ok(None);
            }
            Some(active) => Some(active),
            None => select_ready_job(&transaction, &request.scope)?.map(ActiveJobRecovery::Reoffer),
        };
        let Some(selected) = selected else {
            insert_drive_receipt(
                &transaction,
                &scope_key,
                &request.request_id,
                &request_digest,
                None,
            )?;
            transaction.commit().map_err(sql_error)?;
            return Ok(None);
        };

        let (receipt, recovered) = match selected {
            ActiveJobRecovery::Wait => unreachable!("wait returns before dispatch selection"),
            ActiveJobRecovery::Reoffer(job) if job.state == ExecutionJobState::Queued => {
                (claim_ready_job(&transaction, request, job)?, false)
            }
            ActiveJobRecovery::Reoffer(job) => {
                (recovered_receipt(&transaction, request, job)?, true)
            }
            ActiveJobRecovery::Replace { job, lease } => (
                replacement_receipt(&transaction, request, &job, &lease)?,
                true,
            ),
        };
        record_offer(&transaction, request, &scope_key, &receipt)?;
        if !recovered {
            record_fair_dispatch(&transaction, &scope_key, &receipt.job)?;
        }
        insert_drive_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
            Some(&receipt),
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(Some(receipt))
    }

    /// Claims one policy-eligible failed Job as exactly one higher attempt.
    ///
    /// Receipt replay is resolved before current state or Worker capacity. The
    /// transaction changes `Failed` directly to `Leased`, rotates dispatch and
    /// Registry authority, and never routes the Job through `Queued`.
    ///
    /// # Errors
    ///
    /// Rejects changed replay, cancelled/terminal/non-failed Jobs, invalid
    /// retry policy or time, foreign Worker authority, and storage failures.
    pub fn retry_failed(
        &mut self,
        request: &RepositorySchedulerRetryRequest,
    ) -> Result<Option<RepositorySchedulerClaimReceipt>, StorageError> {
        validate_retry_request(request)?;
        let scope_key = repository_scope_key(&request.scope);
        let request_digest = retry_request_digest(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let DriveReceiptReplay::Stored(mut receipt) = replay_retry_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
        )? {
            if let Some(receipt) = receipt.as_mut() {
                receipt.replayed = true;
            }
            transaction.commit().map_err(sql_error)?;
            return Ok(*receipt);
        }
        let current = load_repository_job(&transaction, &request.scope, &request.job_id)?;
        if current.state != ExecutionJobState::Failed || current.cancellation.is_some() {
            return Err(StorageError::invalid_input(
                "repository scheduler retry requires an uncancelled failed Job",
            ));
        }
        let decision = scheduler_retry_decision(
            &current,
            request.retryable_failure,
            request.failed_at_tick,
            request.policy,
        )
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
        let receipt = match decision {
            SchedulerRetryDecision::Retry {
                next_attempt,
                eligible_at_tick,
            } if request.now_tick >= eligible_at_tick => Some(retry_failed_receipt(
                &transaction,
                request,
                &current,
                next_attempt,
            )?),
            SchedulerRetryDecision::Retry { .. }
            | SchedulerRetryDecision::Exhausted
            | SchedulerRetryDecision::PermanentFailure => None,
        };
        if let Some(receipt) = receipt.as_ref() {
            let claim = retry_claim_request(request);
            record_offer(&transaction, &claim, &scope_key, receipt)?;
        }
        insert_retry_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
            receipt.as_ref(),
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    /// Lists a bounded repository cut for cancellation/restart orchestration.
    ///
    /// # Errors
    ///
    /// Rejects malformed scope/state filters and storage corruption.
    pub fn list_jobs(
        &self,
        scope: &RepositorySchedulerScope,
        states: &[ExecutionJobState],
    ) -> Result<Vec<ExecutionJobRecord>, StorageError> {
        validate_scope(scope)?;
        list_repository_jobs(self.storage.connection()?, scope, states)
    }

    /// Resolves the repository owner of one globally unique durable Job.
    ///
    /// # Errors
    ///
    /// Rejects malformed/missing Job identity and stored scope corruption.
    pub fn scope_for_job(
        &self,
        job_id: &ExecutionJobId,
    ) -> Result<RepositorySchedulerScope, StorageError> {
        validate_id(&job_id.0, "job_", "jobId")?;
        let scope = self
            .storage
            .connection()?
            .query_row(
                "SELECT organization_id, workspace_id, project_id, repository_id
                 FROM scheduler_execution_jobs WHERE job_id = ?1",
                [&job_id.0],
                |row| {
                    Ok(RepositorySchedulerScope {
                        organization_id: OrganizationId(row.get(0)?),
                        workspace_id: WorkspaceId(row.get(1)?),
                        project_id: ProjectId(row.get(2)?),
                        repository_id: RepositoryId(row.get(3)?),
                    })
                },
            )
            .optional()
            .map_err(sql_error)?
            .ok_or_else(|| {
                StorageError::invalid_input("repository execution job does not exist")
            })?;
        validate_scope(&scope)?;
        Ok(scope)
    }

    /// Persists one cancellation and either returns exact Worker cancel
    /// authority or terminalizes work which never reached an accepted dispatch.
    ///
    /// # Errors
    ///
    /// Rejects changed replay bodies, stale revisions, cross-repository jobs,
    /// corrupt lease/dispatch authority, and storage failures.
    pub fn request_cancellation(
        &mut self,
        request: &RepositorySchedulerCancellationRequest,
    ) -> Result<RepositorySchedulerCancellationReceipt, StorageError> {
        validate_cancellation_request(request)?;
        let scope_key = repository_scope_key(&request.scope);
        let request_digest = digest(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if let Some(mut receipt) = replay_cancel_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
        )? {
            receipt.replayed = true;
            transaction.commit().map_err(sql_error)?;
            return Ok(receipt);
        }
        let current = load_repository_job(&transaction, &request.scope, &request.job_id)?;
        let source_state = current.state;
        let cancelled = cancel_execution_job_in_transaction(
            &transaction,
            &ExecutionJobCancellationRequest {
                scope: current.scope.clone(),
                job_id: current.job_id.clone(),
                request_id: request.request_id.clone(),
                expected_revision: request.expected_revision,
                requested_at: request.requested_at.clone(),
            },
        )?;
        let receipt = cancellation_result(&transaction, request, source_state, cancelled.job)?;
        insert_cancel_receipt(
            &transaction,
            &scope_key,
            &request.request_id,
            &request_digest,
            &receipt,
        )?;
        transaction.commit().map_err(sql_error)?;
        Ok(receipt)
    }

    /// Atomically records a Worker dispatch result and advances the queue.
    /// Accepted authority moves the job to running. A rejected result fails
    /// the same queue attempt and releases its Registry lease.
    ///
    /// # Errors
    ///
    /// Rejects cross-repository jobs, changed message replay, stale revisions,
    /// mismatched lease authority, and storage failures.
    pub fn record_dispatch_result(
        &mut self,
        request: &RepositorySchedulerDispatchResultRequest,
    ) -> Result<RepositorySchedulerDispatchResultReceipt, StorageError> {
        validate_dispatch_request(request)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let current = load_repository_job(&transaction, &request.scope, &request.dispatch.job_id)?;
        let dispatch = record_dispatch_result_in_transaction(&transaction, &request.dispatch)?;
        let authority =
            load_dispatch_authority_in_transaction(&transaction, &request.dispatch.job_id)?;
        let accepted = authority.is_some();
        if dispatch.replayed || dispatch.error.is_some() {
            transaction.commit().map_err(sql_error)?;
            return Ok(RepositorySchedulerDispatchResultReceipt {
                job: current,
                dispatch,
                accepted,
            });
        }
        let leased_revision = load_leased_queue_revision(&transaction, &current)?;
        let identity = dispatch_result_identity(request);
        let job = if let Some(authority) = authority {
            require_job_lease(&current, authority.lease())?;
            transition_execution_job_in_transaction(
                &transaction,
                &ExecutionJobTransitionRequest {
                    scope: current.scope,
                    job_id: request.dispatch.job_id.clone(),
                    request_id: derived_request_id(
                        DISPATCH_QUEUE_ID_NAMESPACE,
                        identity.as_bytes(),
                    ),
                    expected_revision: leased_revision,
                    from: ExecutionJobState::Leased,
                    to: ExecutionJobState::Running,
                    occurred_at: request.dispatch.checked_at.clone(),
                },
            )?
            .job
        } else {
            finish_execution_lease_in_transaction(
                &transaction,
                &ExecutionLeaseTerminalRequest {
                    job_id: request.dispatch.job_id.clone(),
                    lease_id: request.dispatch.lease_id.clone(),
                    worker_id: request.dispatch.worker_id.clone(),
                    worker_instance_id: request.dispatch.worker_instance_id.clone(),
                    attempt: request.dispatch.attempt,
                    fencing_token: request.dispatch.fencing_token.clone(),
                    outcome: ExecutionLeaseTerminalOutcome::Failed,
                    terminal_at: request.dispatch.checked_at.clone(),
                    request_id: derived_request_id(
                        DISPATCH_LEASE_TERMINAL_ID_NAMESPACE,
                        identity.as_bytes(),
                    ),
                },
            )?;
            transition_execution_job_in_transaction(
                &transaction,
                &ExecutionJobTransitionRequest {
                    scope: current.scope,
                    job_id: request.dispatch.job_id.clone(),
                    request_id: derived_request_id(
                        DISPATCH_QUEUE_ID_NAMESPACE,
                        identity.as_bytes(),
                    ),
                    expected_revision: leased_revision,
                    from: ExecutionJobState::Leased,
                    to: ExecutionJobState::Failed,
                    occurred_at: request.dispatch.checked_at.clone(),
                },
            )?
            .job
        };
        transaction.commit().map_err(sql_error)?;
        Ok(RepositorySchedulerDispatchResultReceipt {
            job,
            dispatch,
            accepted,
        })
    }

    /// Atomically commits one exact terminal queue state and releases the
    /// matching Registry lease. Exact replay returns the original queue fact.
    ///
    /// # Errors
    ///
    /// Rejects cross-repository jobs, stale revisions/fences, illegal source
    /// states, changed receipts, and storage failures.
    pub fn settle_terminal(
        &mut self,
        request: &RepositorySchedulerTerminalRequest,
    ) -> Result<RepositorySchedulerTerminalReceipt, StorageError> {
        validate_scope(&request.scope)?;
        let transaction = self
            .storage
            .connection_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        let current = load_repository_job(&transaction, &request.scope, &request.terminal.job_id)?;
        let target = terminal_queue_state(request.terminal.outcome);
        let (from, expected_revision) = terminal_transition_authority(
            &transaction,
            &current,
            &request.terminal.request_id,
            target,
        )?;
        let queue_receipt = transition_execution_job_in_transaction(
            &transaction,
            &ExecutionJobTransitionRequest {
                scope: current.scope,
                job_id: request.terminal.job_id.clone(),
                request_id: request.terminal.request_id.clone(),
                expected_revision,
                from,
                to: target,
                occurred_at: request.terminal.terminal_at.clone(),
            },
        )?;
        let lease_inserted =
            finish_execution_lease_in_transaction(&transaction, &request.terminal)?;
        transaction.commit().map_err(sql_error)?;
        Ok(RepositorySchedulerTerminalReceipt {
            job: queue_receipt.job,
            lease_inserted,
            replayed: queue_receipt.replayed,
        })
    }
}

fn load_repository_job(
    connection: &Connection,
    scope: &RepositorySchedulerScope,
    job_id: &ExecutionJobId,
) -> Result<ExecutionJobRecord, StorageError> {
    let stored = connection
        .query_row(
            "SELECT job_id, organization_id, workspace_id, project_id, repository_id,
                    product_session_id, delivery_id, stage_run_id, submission_request_id,
                    payload_digest, dispatch_payload, state, attempt, revision, submitted_at,
                    updated_at, cancellation_request_id, cancellation_requested_at
             FROM scheduler_execution_jobs
             WHERE job_id = ?1 AND organization_id = ?2 AND workspace_id = ?3
               AND project_id = ?4 AND repository_id = ?5",
            params![
                job_id.0,
                scope.organization_id.0,
                scope.workspace_id.0,
                scope.project_id.0,
                scope.repository_id.0,
            ],
            stored_job_from_row,
        )
        .optional()
        .map_err(sql_error)?
        .ok_or_else(|| StorageError::invalid_input("repository execution job does not exist"))?;
    complete_record(connection, stored)
}

pub(crate) fn load_execution_job_by_id(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<Option<ExecutionJobRecord>, StorageError> {
    ensure_execution_queue_schema(connection)?;
    connection
        .query_row(
            "SELECT job_id, organization_id, workspace_id, project_id, repository_id,
                    product_session_id, delivery_id, stage_run_id, submission_request_id,
                    payload_digest, dispatch_payload, state, attempt, revision, submitted_at,
                    updated_at, cancellation_request_id, cancellation_requested_at
             FROM scheduler_execution_jobs WHERE job_id = ?1",
            [&job_id.0],
            stored_job_from_row,
        )
        .optional()
        .map_err(sql_error)?
        .map(|stored| complete_record(connection, stored))
        .transpose()
}

fn cancellation_result(
    connection: &Connection,
    request: &RepositorySchedulerCancellationRequest,
    source_state: ExecutionJobState,
    cancelling: ExecutionJobRecord,
) -> Result<RepositorySchedulerCancellationReceipt, StorageError> {
    if let Some(authority) = load_dispatch_authority_in_transaction(connection, &cancelling.job_id)?
    {
        let lease = authority.lease().clone();
        require_job_lease(&cancelling, &lease)?;
        return Ok(RepositorySchedulerCancellationReceipt {
            job: cancelling,
            lease: Some(lease),
            worker_session_id: Some(authority.worker_session_id().clone()),
            message_id: Some(derived_message_id(
                CANCEL_MESSAGE_ID_NAMESPACE,
                cancellation_identity(request).as_bytes(),
            )),
            request_id: request.request_id.clone(),
            replayed: false,
        });
    }

    let lease = load_lease_in_transaction(connection, &cancelling.job_id)?;
    if source_state != ExecutionJobState::Queued && lease.is_none() {
        return Err(StorageError::adapter(
            "active cancellation has no Registry lease authority",
        ));
    }
    if let Some(lease) = lease.as_ref() {
        require_job_lease(&cancelling, lease)?;
        finish_execution_lease_in_transaction(
            connection,
            &ExecutionLeaseTerminalRequest {
                job_id: lease.job_id.clone(),
                lease_id: lease.lease_id.clone(),
                worker_id: lease.worker_id.clone(),
                worker_instance_id: lease.worker_instance_id.clone(),
                attempt: lease.attempt,
                fencing_token: lease.fencing_token.clone(),
                outcome: ExecutionLeaseTerminalOutcome::Cancelled,
                terminal_at: request.requested_at.clone(),
                request_id: derived_request_id(
                    CANCEL_LEASE_TERMINAL_ID_NAMESPACE,
                    cancellation_identity(request).as_bytes(),
                ),
            },
        )?;
    }
    let terminal = transition_execution_job_in_transaction(
        connection,
        &ExecutionJobTransitionRequest {
            scope: cancelling.scope.clone(),
            job_id: cancelling.job_id.clone(),
            request_id: derived_request_id(
                CANCEL_QUEUE_TERMINAL_ID_NAMESPACE,
                cancellation_identity(request).as_bytes(),
            ),
            expected_revision: cancelling.revision,
            from: ExecutionJobState::Cancelling,
            to: ExecutionJobState::Failed,
            occurred_at: request.requested_at.clone(),
        },
    )?;
    Ok(RepositorySchedulerCancellationReceipt {
        job: terminal.job,
        lease,
        worker_session_id: None,
        message_id: None,
        request_id: request.request_id.clone(),
        replayed: false,
    })
}

fn require_job_lease(
    job: &ExecutionJobRecord,
    lease: &ExecutionLeaseRecord,
) -> Result<(), StorageError> {
    if lease.job_id != job.job_id
        || lease.payload_digest != job.payload_digest
        || lease.attempt != job.attempt
    {
        return Err(StorageError::adapter(
            "repository queue job differs from Registry lease authority",
        ));
    }
    Ok(())
}

const fn terminal_queue_state(outcome: ExecutionLeaseTerminalOutcome) -> ExecutionJobState {
    match outcome {
        ExecutionLeaseTerminalOutcome::Completed => ExecutionJobState::Completed,
        ExecutionLeaseTerminalOutcome::Cancelled | ExecutionLeaseTerminalOutcome::Failed => {
            ExecutionJobState::Failed
        }
    }
}

fn cancellation_identity(request: &RepositorySchedulerCancellationRequest) -> String {
    format!("{}\u{1f}{}", request.job_id.0, request.request_id.0)
}

fn replay_cancel_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<Option<RepositorySchedulerCancellationReceipt>, StorageError> {
    let Some((stored_digest, response_json)) = connection
        .query_row(
            "SELECT request_digest, response_json FROM repository_scheduler_cancel_receipts
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
    serde_json::from_str(&response_json)
        .map(Some)
        .map_err(|_| StorageError::adapter("scheduler cancellation receipt is corrupt"))
}

fn insert_cancel_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    request_digest: &str,
    response: &RepositorySchedulerCancellationReceipt,
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO repository_scheduler_cancel_receipts
                (scope_key, request_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                scope_key,
                request_id.0,
                request_digest,
                serde_json::to_string(response).map_err(|_| {
                    StorageError::adapter("scheduler cancellation receipt encode failed")
                })?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn validate_cancellation_request(
    request: &RepositorySchedulerCancellationRequest,
) -> Result<(), StorageError> {
    validate_scope(&request.scope)?;
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_instant(&request.requested_at, "requestedAt")?;
    if request.expected_revision == 0 {
        return Err(StorageError::invalid_input(
            "scheduler cancellation revision is invalid",
        ));
    }
    Ok(())
}

fn validate_dispatch_request(
    request: &RepositorySchedulerDispatchResultRequest,
) -> Result<(), StorageError> {
    validate_scope(&request.scope)
}

fn dispatch_result_identity(request: &RepositorySchedulerDispatchResultRequest) -> String {
    format!(
        "{}\u{1f}{}",
        request.dispatch.job_id.0, request.dispatch.request_id.0
    )
}

fn load_leased_queue_revision(
    connection: &Connection,
    job: &ExecutionJobRecord,
) -> Result<u64, StorageError> {
    let mut statement = connection
        .prepare(
            "SELECT response_json FROM scheduler_execution_job_receipts
             WHERE operation = 'transition' AND job_id = ?1 ORDER BY rowid DESC",
        )
        .map_err(sql_error)?;
    let responses = statement
        .query_map([&job.job_id.0], |row| row.get::<_, String>(0))
        .map_err(sql_error)?;
    for response in responses {
        let response = response.map_err(sql_error)?;
        let receipt: crate::ExecutionJobMutationReceipt = serde_json::from_str(&response)
            .map_err(|_| StorageError::adapter("scheduler queue receipt is corrupt"))?;
        if receipt.job.job_id == job.job_id
            && receipt.job.payload_digest == job.payload_digest
            && receipt.job.attempt == job.attempt
            && receipt.job.state == ExecutionJobState::Leased
        {
            return Ok(receipt.job.revision);
        }
    }
    Err(StorageError::adapter(
        "repository dispatch has no durable leased queue receipt",
    ))
}

fn terminal_transition_authority(
    connection: &Connection,
    current: &ExecutionJobRecord,
    request_id: &RequestId,
    target: ExecutionJobState,
) -> Result<(ExecutionJobState, u64), StorageError> {
    let stored = connection
        .query_row(
            "SELECT response_json FROM scheduler_execution_job_receipts
             WHERE operation = 'transition' AND job_id = ?1 AND request_id = ?2",
            params![current.job_id.0, request_id.0],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_error)?;
    if let Some(stored) = stored {
        let receipt: crate::ExecutionJobMutationReceipt = serde_json::from_str(&stored)
            .map_err(|_| StorageError::adapter("scheduler terminal receipt is corrupt"))?;
        if receipt.job.job_id != current.job_id || receipt.job.state != target {
            return Err(StorageError::invalid_input(
                "scheduler terminal receipt differs from requested outcome",
            ));
        }
        let expected_revision = receipt
            .job
            .revision
            .checked_sub(1)
            .ok_or_else(|| StorageError::adapter("scheduler terminal revision is corrupt"))?;
        let from = if receipt.job.cancellation.is_some() {
            ExecutionJobState::Cancelling
        } else {
            ExecutionJobState::Running
        };
        return Ok((from, expected_revision));
    }
    if !matches!(
        current.state,
        ExecutionJobState::Running | ExecutionJobState::Cancelling
    ) {
        return Err(StorageError::invalid_input(
            "execution job terminal source state is invalid",
        ));
    }
    Ok((current.state, current.revision))
}

fn recover_active_job(
    connection: &Connection,
    request: &RepositorySchedulerClaimRequest,
    scope_key: &str,
) -> Result<Option<ActiveJobRecovery>, StorageError> {
    let jobs = list_repository_jobs(
        connection,
        &request.scope,
        &[ExecutionJobState::Leased, ExecutionJobState::Running],
    )?;
    for job in jobs {
        let offered = connection
            .query_row(
                "SELECT 1 FROM repository_scheduler_offers
                 WHERE scope_key = ?1 AND scheduler_generation = ?2 AND job_id = ?3",
                params![scope_key, request.scheduler_generation, job.job_id.0],
                |_| Ok(()),
            )
            .optional()
            .map_err(sql_error)?
            .is_some();
        if offered {
            continue;
        }
        let lease = load_lease_in_transaction(connection, &job.job_id)?
            .ok_or_else(|| StorageError::adapter("active queue job has no Registry lease"))?;
        require_job_lease(&job, &lease)?;
        if lease.worker_id != request.worker_id {
            continue;
        }
        if lease.worker_instance_id == request.worker_instance_id {
            return Ok(Some(ActiveJobRecovery::Reoffer(job)));
        }
        if request.issued_at.0 < lease.expires_at.0 {
            return Ok(Some(ActiveJobRecovery::Wait));
        }
        return Ok(Some(ActiveJobRecovery::Replace { job, lease }));
    }
    Ok(None)
}

fn replacement_receipt(
    connection: &Connection,
    request: &RepositorySchedulerClaimRequest,
    job: &ExecutionJobRecord,
    previous_lease: &ExecutionLeaseRecord,
) -> Result<RepositorySchedulerClaimReceipt, StorageError> {
    if previous_lease.worker_id != request.worker_id
        || previous_lease.worker_instance_id == request.worker_instance_id
        || previous_lease.job_id != job.job_id
    {
        return Err(StorageError::invalid_input(
            "execution replacement does not change the Worker process instance",
        ));
    }
    let (predecessor_worker_session_id, predecessor_slot) =
        predecessor_runtime_authority(connection, &job.job_id)?;
    let replacement = replacement_dispatch_payload(job)?;
    let logical_job_digest = logical_dispatch_digest(&job.dispatch_payload)?;
    let next_identity = format!("{}\u{1f}{}", job.job_id.0, replacement.attempt);
    finish_execution_lease_in_transaction(
        connection,
        &ExecutionLeaseTerminalRequest {
            job_id: previous_lease.job_id.clone(),
            lease_id: previous_lease.lease_id.clone(),
            worker_id: previous_lease.worker_id.clone(),
            worker_instance_id: previous_lease.worker_instance_id.clone(),
            attempt: previous_lease.attempt,
            fencing_token: previous_lease.fencing_token.clone(),
            outcome: ExecutionLeaseTerminalOutcome::Failed,
            terminal_at: request.issued_at.clone(),
            request_id: derived_request_id(
                REPLACEMENT_LEASE_TERMINAL_ID_NAMESPACE,
                next_identity.as_bytes(),
            ),
        },
    )?;
    fence_worker_session_for_replacement_in_transaction(
        connection,
        previous_lease,
        &request.issued_at,
    )
    .map_err(|error| StorageError::adapter(format!("Worker slot replacement failed: {error}")))?;
    let queue_request_id = derived_request_id(QUEUE_ID_NAMESPACE, next_identity.as_bytes());
    let replaced = replace_execution_job_attempt_in_transaction(
        connection,
        job,
        &queue_request_id,
        replacement.attempt,
        &replacement.bytes,
        &request.issued_at,
    )?
    .job;
    let identity = job_attempt_identity(&replaced);
    let registry_request_id = derived_request_id(CLAIM_ID_NAMESPACE, &identity);
    let message_id = derived_message_id(MESSAGE_ID_NAMESPACE, &identity);
    let claim = ExecutionLeaseClaim {
        expires_at: request.expires_at.clone(),
        fencing_token: FencingToken(replaced.attempt.to_string()),
        issued_at: request.issued_at.clone(),
        job_id: replaced.job_id.clone(),
        lease_id: derived_lease_id(LEASE_ID_NAMESPACE, &identity),
        message_id: message_id.clone(),
        payload_digest: replaced.payload_digest.clone(),
        request_id: registry_request_id.clone(),
        worker_id: request.worker_id.clone(),
        worker_instance_id: request.worker_instance_id.clone(),
        attempt: replaced.attempt,
    };
    let lease_receipt = claim_execution_job_in_transaction(connection, &claim, false)?;
    if lease_receipt.status != LeaseWriteStatus::Accepted {
        return Err(StorageError::invalid_input(
            "repository scheduler replacement Registry claim was rejected",
        ));
    }
    let lease = lease_receipt
        .lease
        .ok_or_else(|| StorageError::adapter("accepted replacement Registry claim has no lease"))?;
    insert_execution_scope_replacement(
        connection,
        &NewExecutionScopeReplacement {
            receipt_id: &queue_request_id,
            logical_job_digest: &logical_job_digest,
            scope: &job.scope,
            stage_run_id: job.stage_run_id.as_ref(),
            predecessor_lease: previous_lease,
            predecessor_worker_session_id: predecessor_worker_session_id.as_ref(),
            predecessor_slot: predecessor_slot.as_ref(),
            successor_lease: &lease,
            created_at: &request.issued_at,
        },
    )?;
    Ok(RepositorySchedulerClaimReceipt {
        job: replaced,
        lease,
        message_id,
        request_id: registry_request_id,
        replayed: false,
        recovered: true,
    })
}

fn predecessor_runtime_authority(
    connection: &Connection,
    job_id: &ExecutionJobId,
) -> Result<(Option<WorkerSessionId>, Option<WorkerSlotAuthority>), StorageError> {
    let dispatch = load_dispatch_authority_in_transaction(connection, job_id)?;
    let slot = dispatch
        .as_ref()
        .map(|dispatch| {
            load_slot_in_transaction(connection, dispatch.worker_session_id()).map_err(|error| {
                StorageError::adapter(format!("Worker replacement slot load failed: {error}"))
            })
        })
        .transpose()?
        .flatten()
        .map(|slot| slot.authority);
    let worker_session_id = dispatch.map(|dispatch| dispatch.worker_session_id().clone());
    Ok((worker_session_id, slot))
}

fn retry_failed_receipt(
    connection: &Connection,
    request: &RepositorySchedulerRetryRequest,
    job: &ExecutionJobRecord,
    next_attempt: u64,
) -> Result<RepositorySchedulerClaimReceipt, StorageError> {
    let predecessor_lease = load_lease_in_transaction(connection, &job.job_id)?
        .ok_or_else(|| StorageError::adapter("failed execution Job has no Registry lease"))?;
    require_job_lease(job, &predecessor_lease)?;
    let (predecessor_worker_session_id, predecessor_slot) =
        predecessor_runtime_authority(connection, &job.job_id)?;
    let replacement = replacement_dispatch_payload(job)?;
    if replacement.attempt != next_attempt {
        return Err(StorageError::adapter(
            "scheduler retry decision differs from replacement payload",
        ));
    }
    fence_worker_session_for_replacement_in_transaction(
        connection,
        &predecessor_lease,
        &request.issued_at,
    )
    .map_err(|error| {
        StorageError::adapter(format!("Worker retry slot replacement failed: {error}"))
    })?;
    let logical_job_digest = logical_dispatch_digest(&job.dispatch_payload)?;
    let identity = format!("{}\u{1f}{next_attempt}", job.job_id.0);
    let queue_request_id = derived_request_id(QUEUE_ID_NAMESPACE, identity.as_bytes());
    let replaced = replace_execution_job_attempt_in_transaction(
        connection,
        job,
        &queue_request_id,
        next_attempt,
        &replacement.bytes,
        &request.issued_at,
    )?
    .job;
    let attempt_identity = job_attempt_identity(&replaced);
    let registry_request_id = derived_request_id(CLAIM_ID_NAMESPACE, &attempt_identity);
    let message_id = derived_message_id(MESSAGE_ID_NAMESPACE, &attempt_identity);
    let lease_receipt = claim_execution_job_in_transaction(
        connection,
        &ExecutionLeaseClaim {
            expires_at: request.expires_at.clone(),
            fencing_token: FencingToken(next_attempt.to_string()),
            issued_at: request.issued_at.clone(),
            job_id: replaced.job_id.clone(),
            lease_id: derived_lease_id(LEASE_ID_NAMESPACE, &attempt_identity),
            message_id: message_id.clone(),
            payload_digest: replaced.payload_digest.clone(),
            request_id: registry_request_id.clone(),
            worker_id: request.worker_id.clone(),
            worker_instance_id: request.worker_instance_id.clone(),
            attempt: next_attempt,
        },
        false,
    )?;
    if lease_receipt.status != LeaseWriteStatus::Accepted {
        return Err(StorageError::invalid_input(
            "repository scheduler retry Registry claim was rejected",
        ));
    }
    let lease = lease_receipt
        .lease
        .ok_or_else(|| StorageError::adapter("accepted retry Registry claim has no lease"))?;
    insert_execution_scope_replacement(
        connection,
        &NewExecutionScopeReplacement {
            receipt_id: &queue_request_id,
            logical_job_digest: &logical_job_digest,
            scope: &job.scope,
            stage_run_id: job.stage_run_id.as_ref(),
            predecessor_lease: &predecessor_lease,
            predecessor_worker_session_id: predecessor_worker_session_id.as_ref(),
            predecessor_slot: predecessor_slot.as_ref(),
            successor_lease: &lease,
            created_at: &request.issued_at,
        },
    )?;
    Ok(RepositorySchedulerClaimReceipt {
        job: replaced,
        lease,
        message_id,
        request_id: registry_request_id,
        replayed: false,
        recovered: false,
    })
}

fn select_ready_job(
    connection: &Connection,
    scope: &RepositorySchedulerScope,
) -> Result<Option<ExecutionJobRecord>, StorageError> {
    let scope_key = repository_scope_key(scope);
    let stored = connection
        .query_row(
            "SELECT j.job_id, j.organization_id, j.workspace_id, j.project_id, j.repository_id,
                    j.product_session_id, j.delivery_id, j.stage_run_id,
                    j.submission_request_id, j.payload_digest, j.dispatch_payload, j.state,
                    j.attempt, j.revision, j.submitted_at, j.updated_at,
                    j.cancellation_request_id, j.cancellation_requested_at
             FROM scheduler_execution_jobs j
             LEFT JOIN repository_scheduler_fairness f
               ON f.scope_key = ?1
              AND f.product_session_id = j.product_session_id
              AND f.delivery_key = COALESCE(j.delivery_id, '')
             WHERE j.organization_id = ?2 AND j.workspace_id = ?3
               AND j.project_id = ?4 AND j.repository_id = ?5
               AND j.state = 'queued' AND j.cancellation_request_id IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM scheduler_execution_job_dependencies d
                   LEFT JOIN scheduler_execution_jobs dependency
                     ON dependency.job_id = d.dependency_job_id
                   WHERE d.job_id = j.job_id
                     AND (dependency.job_id IS NULL OR dependency.state != 'completed'
                          OR dependency.cancellation_request_id IS NOT NULL)
               )
               AND NOT EXISTS (
                   SELECT 1 FROM device_execution_reservation_facts device_dispatch
                   WHERE device_dispatch.job_id = j.job_id
               )
             ORDER BY COALESCE(f.last_sequence, 0), j.submitted_at, j.job_id
             LIMIT 1",
            params![
                scope_key,
                scope.organization_id.0,
                scope.workspace_id.0,
                scope.project_id.0,
                scope.repository_id.0,
            ],
            stored_job_from_row,
        )
        .optional()
        .map_err(sql_error)?;
    stored
        .map(|row| complete_record(connection, row))
        .transpose()
}

fn claim_ready_job(
    connection: &Connection,
    request: &RepositorySchedulerClaimRequest,
    job: ExecutionJobRecord,
) -> Result<RepositorySchedulerClaimReceipt, StorageError> {
    let identity = job_attempt_identity(&job);
    let registry_request_id = derived_request_id(CLAIM_ID_NAMESPACE, &identity);
    let queue_request_id = derived_request_id(QUEUE_ID_NAMESPACE, &identity);
    let message_id = derived_message_id(MESSAGE_ID_NAMESPACE, &identity);
    let lease_id = derived_lease_id(LEASE_ID_NAMESPACE, &identity);
    let claim = ExecutionLeaseClaim {
        expires_at: request.expires_at.clone(),
        fencing_token: FencingToken(job.attempt.to_string()),
        issued_at: request.issued_at.clone(),
        job_id: job.job_id.clone(),
        lease_id,
        message_id: message_id.clone(),
        payload_digest: job.payload_digest.clone(),
        request_id: registry_request_id.clone(),
        worker_id: request.worker_id.clone(),
        worker_instance_id: request.worker_instance_id.clone(),
        attempt: job.attempt,
    };
    let lease_receipt = claim_execution_job_in_transaction(connection, &claim, false)?;
    if lease_receipt.status != LeaseWriteStatus::Accepted {
        return Err(StorageError::invalid_input(
            "repository scheduler Registry claim was rejected",
        ));
    }
    let lease = lease_receipt
        .lease
        .ok_or_else(|| StorageError::adapter("accepted Registry claim has no lease"))?;
    let leased = transition_execution_job_in_transaction(
        connection,
        &ExecutionJobTransitionRequest {
            scope: job.scope.clone(),
            job_id: job.job_id,
            request_id: queue_request_id,
            expected_revision: job.revision,
            from: ExecutionJobState::Queued,
            to: ExecutionJobState::Leased,
            occurred_at: request.issued_at.clone(),
        },
    )?;
    Ok(RepositorySchedulerClaimReceipt {
        job: leased.job,
        lease,
        message_id,
        request_id: registry_request_id,
        replayed: false,
        recovered: false,
    })
}

fn recovered_receipt(
    connection: &Connection,
    request: &RepositorySchedulerClaimRequest,
    job: ExecutionJobRecord,
) -> Result<RepositorySchedulerClaimReceipt, StorageError> {
    let lease = load_lease_in_transaction(connection, &job.job_id)?
        .ok_or_else(|| StorageError::adapter("active queue job has no Registry lease"))?;
    if lease.worker_id != request.worker_id
        || lease.worker_instance_id != request.worker_instance_id
        || lease.job_id != job.job_id
        || lease.payload_digest != job.payload_digest
        || lease.attempt != job.attempt
    {
        return Err(StorageError::invalid_input(
            "active queue job differs from its Registry lease",
        ));
    }
    let identity = job_attempt_identity(&job);
    Ok(RepositorySchedulerClaimReceipt {
        job,
        lease,
        message_id: derived_message_id(MESSAGE_ID_NAMESPACE, &identity),
        request_id: derived_request_id(CLAIM_ID_NAMESPACE, &identity),
        replayed: false,
        recovered: true,
    })
}

fn record_offer(
    connection: &Connection,
    request: &RepositorySchedulerClaimRequest,
    scope_key: &str,
    receipt: &RepositorySchedulerClaimReceipt,
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO repository_scheduler_offers
                (scope_key, scheduler_generation, job_id, message_id, offered_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                scope_key,
                request.scheduler_generation,
                receipt.job.job_id.0,
                receipt.message_id.0,
                request.issued_at.0,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn record_fair_dispatch(
    connection: &Connection,
    scope_key: &str,
    job: &ExecutionJobRecord,
) -> Result<(), StorageError> {
    let next = connection
        .query_row(
            "SELECT next_sequence FROM repository_scheduler_state WHERE scope_key = ?1",
            [scope_key],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sql_error)?
        .unwrap_or(1);
    connection
        .execute(
            "INSERT INTO repository_scheduler_fairness
                (scope_key, product_session_id, delivery_key, last_sequence)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope_key, product_session_id, delivery_key)
             DO UPDATE SET last_sequence = excluded.last_sequence",
            params![
                scope_key,
                job.scope.product_session_id.0,
                job.scope
                    .delivery_id
                    .as_ref()
                    .map_or("", |id| id.0.as_str()),
                next,
            ],
        )
        .map_err(sql_error)?;
    let following = next
        .checked_add(1)
        .ok_or_else(|| StorageError::adapter("scheduler fairness sequence overflowed"))?;
    connection
        .execute(
            "INSERT INTO repository_scheduler_state (scope_key, next_sequence)
             VALUES (?1, ?2)
             ON CONFLICT(scope_key) DO UPDATE SET next_sequence = excluded.next_sequence",
            params![scope_key, following],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn list_repository_jobs(
    connection: &Connection,
    scope: &RepositorySchedulerScope,
    states: &[ExecutionJobState],
) -> Result<Vec<ExecutionJobRecord>, StorageError> {
    validate_scope(scope)?;
    if states.len() > 6 {
        return Err(StorageError::invalid_input(
            "repository scheduler state filter is invalid",
        ));
    }
    let includes = |state| i64::from(states.is_empty() || states.contains(&state));
    let mut statement = connection
        .prepare(
            "SELECT job_id, organization_id, workspace_id, project_id, repository_id,
                    product_session_id, delivery_id, stage_run_id, submission_request_id,
                    payload_digest, dispatch_payload, state, attempt, revision, submitted_at,
                    updated_at, cancellation_request_id, cancellation_requested_at
             FROM scheduler_execution_jobs
             WHERE organization_id = ?1 AND workspace_id = ?2 AND project_id = ?3
               AND repository_id = ?4
               AND ((?5 = 1 AND state = 'queued') OR (?6 = 1 AND state = 'leased')
                 OR (?7 = 1 AND state = 'running') OR (?8 = 1 AND state = 'cancelling')
                 OR (?9 = 1 AND state = 'completed') OR (?10 = 1 AND state = 'failed'))
             ORDER BY submitted_at, job_id
             LIMIT ?11",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(
            params![
                scope.organization_id.0,
                scope.workspace_id.0,
                scope.project_id.0,
                scope.repository_id.0,
                includes(ExecutionJobState::Queued),
                includes(ExecutionJobState::Leased),
                includes(ExecutionJobState::Running),
                includes(ExecutionJobState::Cancelling),
                includes(ExecutionJobState::Completed),
                includes(ExecutionJobState::Failed),
                i64::try_from(MAX_REPOSITORY_JOBS)
                    .map_err(|_| StorageError::adapter("scheduler job limit is invalid"))?,
            ],
            stored_job_from_row,
        )
        .map_err(sql_error)?;
    rows.map(|row| {
        row.map_err(sql_error)
            .and_then(|row| complete_record(connection, row))
    })
    .collect()
}

fn replay_drive_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<DriveReceiptReplay, StorageError> {
    let Some((stored_digest, response_json)) = connection
        .query_row(
            "SELECT request_digest, response_json FROM repository_scheduler_drive_receipts
             WHERE scope_key = ?1 AND request_id = ?2",
            params![scope_key, request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?
    else {
        return Ok(DriveReceiptReplay::Missing);
    };
    if stored_digest != request_digest {
        return Err(StorageError::request_conflict(request_id));
    }
    serde_json::from_str(&response_json)
        .map(Box::new)
        .map(DriveReceiptReplay::Stored)
        .map_err(|_| StorageError::adapter("scheduler drive receipt is corrupt"))
}

fn replay_retry_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    request_digest: &str,
) -> Result<DriveReceiptReplay, StorageError> {
    let Some((stored_digest, response_json)) = connection
        .query_row(
            "SELECT request_digest, response_json FROM repository_scheduler_retry_receipts
             WHERE scope_key = ?1 AND request_id = ?2",
            params![scope_key, request_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(sql_error)?
    else {
        return Ok(DriveReceiptReplay::Missing);
    };
    if stored_digest != request_digest {
        return Err(StorageError::request_conflict(request_id));
    }
    serde_json::from_str(&response_json)
        .map(Box::new)
        .map(DriveReceiptReplay::Stored)
        .map_err(|_| StorageError::adapter("scheduler retry receipt is corrupt"))
}

fn insert_drive_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    request_digest: &str,
    response: Option<&RepositorySchedulerClaimReceipt>,
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO repository_scheduler_drive_receipts
                (scope_key, request_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                scope_key,
                request_id.0,
                request_digest,
                serde_json::to_string(&response)
                    .map_err(|_| StorageError::adapter("scheduler receipt encode failed"))?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn insert_retry_receipt(
    connection: &Connection,
    scope_key: &str,
    request_id: &RequestId,
    request_digest: &str,
    response: Option<&RepositorySchedulerClaimReceipt>,
) -> Result<(), StorageError> {
    connection
        .execute(
            "INSERT INTO repository_scheduler_retry_receipts
                (scope_key, request_id, request_digest, response_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                scope_key,
                request_id.0,
                request_digest,
                serde_json::to_string(&response)
                    .map_err(|_| StorageError::adapter("scheduler retry receipt encode failed"))?,
            ],
        )
        .map_err(sql_error)?;
    Ok(())
}

fn validate_claim_request(request: &RepositorySchedulerClaimRequest) -> Result<(), StorageError> {
    validate_scope(&request.scope)?;
    validate_id(&request.request_id.0, "req_", "requestId")?;
    validate_id(&request.worker_id.0, "wrk_", "workerId")?;
    validate_id(&request.worker_instance_id.0, "wki_", "workerInstanceId")?;
    if request.scheduler_generation.is_empty()
        || request.scheduler_generation.len() > 128
        || !request
            .scheduler_generation
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(StorageError::invalid_input(
            "scheduler generation is invalid",
        ));
    }
    validate_instant(&request.issued_at, "issuedAt")?;
    validate_instant(&request.expires_at, "expiresAt")?;
    if request.issued_at.0 >= request.expires_at.0 {
        return Err(StorageError::invalid_input(
            "scheduler lease interval is invalid",
        ));
    }
    Ok(())
}

fn validate_retry_request(request: &RepositorySchedulerRetryRequest) -> Result<(), StorageError> {
    validate_id(&request.job_id.0, "job_", "jobId")?;
    validate_claim_request(&retry_claim_request(request))?;
    if request.failed_at_tick > request.now_tick {
        return Err(StorageError::invalid_input(
            "scheduler retry time precedes the failed attempt",
        ));
    }
    Ok(())
}

fn retry_claim_request(
    request: &RepositorySchedulerRetryRequest,
) -> RepositorySchedulerClaimRequest {
    RepositorySchedulerClaimRequest {
        scope: request.scope.clone(),
        request_id: request.request_id.clone(),
        scheduler_generation: request.scheduler_generation.clone(),
        worker_id: request.worker_id.clone(),
        worker_instance_id: request.worker_instance_id.clone(),
        issued_at: request.issued_at.clone(),
        expires_at: request.expires_at.clone(),
    }
}

fn retry_request_digest(request: &RepositorySchedulerRetryRequest) -> Result<String, StorageError> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct RetryDigest<'request> {
        scope: &'request RepositorySchedulerScope,
        job_id: &'request ExecutionJobId,
        request_id: &'request RequestId,
        scheduler_generation: &'request str,
        worker_id: &'request WorkerId,
        worker_instance_id: &'request WorkerInstanceId,
        retryable_failure: bool,
        failed_at_tick: u64,
        now_tick: u64,
        max_attempts: u64,
        initial_backoff_ticks: u64,
        max_backoff_ticks: u64,
        issued_at: &'request Instant,
        expires_at: &'request Instant,
    }
    digest(&RetryDigest {
        scope: &request.scope,
        job_id: &request.job_id,
        request_id: &request.request_id,
        scheduler_generation: &request.scheduler_generation,
        worker_id: &request.worker_id,
        worker_instance_id: &request.worker_instance_id,
        retryable_failure: request.retryable_failure,
        failed_at_tick: request.failed_at_tick,
        now_tick: request.now_tick,
        max_attempts: request.policy.max_attempts,
        initial_backoff_ticks: request.policy.initial_backoff_ticks,
        max_backoff_ticks: request.policy.max_backoff_ticks,
        issued_at: &request.issued_at,
        expires_at: &request.expires_at,
    })
}

fn validate_scope(scope: &RepositorySchedulerScope) -> Result<(), StorageError> {
    validate_id(&scope.organization_id.0, "org_", "organizationId")?;
    validate_id(&scope.workspace_id.0, "wsp_", "workspaceId")?;
    validate_id(&scope.project_id.0, "prj_", "projectId")?;
    validate_id(&scope.repository_id.0, "rep_", "repositoryId")
}

fn validate_id(value: &str, prefix: &str, field: &str) -> Result<(), StorageError> {
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
        Err(StorageError::invalid_input(format!(
            "repository scheduler {field} is not canonical"
        )))
    }
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
            "repository scheduler {field} is invalid"
        )))
    }
}

fn repository_scope_key(scope: &RepositorySchedulerScope) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        scope.organization_id.0, scope.workspace_id.0, scope.project_id.0, scope.repository_id.0
    )
}

fn job_attempt_identity(job: &ExecutionJobRecord) -> Vec<u8> {
    format!("{}\u{1f}{}", job.job_id.0, job.attempt).into_bytes()
}

fn derived_request_id(namespace: &[u8], identity: &[u8]) -> RequestId {
    RequestId(derived_identifier(namespace, identity, "req"))
}

fn derived_message_id(namespace: &[u8], identity: &[u8]) -> ExecutionMessageId {
    ExecutionMessageId(derived_identifier(namespace, identity, "xmsg"))
}

fn derived_lease_id(namespace: &[u8], identity: &[u8]) -> LeaseId {
    LeaseId(derived_identifier(namespace, identity, "lse"))
}

fn derived_identifier(namespace: &[u8], identity: &[u8], prefix: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace);
    hasher.update([0]);
    hasher.update(identity);
    let suffix = hasher
        .finalize()
        .iter()
        .take(26)
        .map(|byte| char::from(IDENTIFIER_ALPHABET[usize::from(byte & 31)]))
        .collect::<String>();
    format!("{prefix}_{suffix}")
}

fn digest(value: &impl Serialize) -> Result<String, StorageError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| StorageError::invalid_input("scheduler request cannot be encoded"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
