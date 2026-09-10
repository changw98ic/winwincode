// SPDX-License-Identifier: Apache-2.0

//! Typed Control Plane adapter for the repository-scoped durable scheduler.
//!
//! Storage owns selection, receipts, queue state, and Registry authority. This
//! module only decodes the already sealed `ExecutionJob` bytes and constructs
//! generated `ExecutionPort` commands from committed authority.

use std::fmt;

use winwincode_delivery::domain::Delivery;
use winwincode_domain::{
    DeliveryId, Instant, RepositoryScope, SchemaVersion, SessionIdentity, WorkItemState, WorkRunId,
};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionJobReplacementAuthority, ExecutionLeaseStamp, ExecutionScope,
    JobCancelMessage, JobCancelMessageKind, JobCancelMessageReason, JobDispatchMessage,
    JobDispatchMessageKind, JobDispatchResultMessage, JobDispatchResultMessageStatus,
};
use winwincode_storage::{
    DispatchResultRequest, DispatchResultStatus, ExecutionJobRecord, ExecutionJobState,
    ExecutionLeaseRecord, ExecutionScopeReplacementAuthority, ProductStateStorage,
    RepositorySchedulerCancellationReceipt, RepositorySchedulerCancellationRequest,
    RepositorySchedulerClaimRequest, RepositorySchedulerDispatchResultReceipt,
    RepositorySchedulerDispatchResultRequest, RepositorySchedulerRetryRequest,
    RepositorySchedulerScope, RepositorySchedulerTerminalReceipt,
    RepositorySchedulerTerminalRequest, SqliteStorage, StorageError,
};

/// Repository scheduler adapter failure before a Worker command is emitted.
#[derive(Debug)]
pub enum RepositoryExecutionSchedulerError {
    Storage(StorageError),
    StaleDeliveryRevision,
    InvalidExecutionJob(&'static str),
    MissingCancellationAuthority(&'static str),
}

impl fmt::Display for RepositoryExecutionSchedulerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => {
                write!(formatter, "repository scheduler storage error: {error}")
            }
            Self::StaleDeliveryRevision => formatter.write_str("Delivery revision is stale"),
            Self::InvalidExecutionJob(field) => {
                write!(formatter, "durable ExecutionJob is invalid: {field}")
            }
            Self::MissingCancellationAuthority(field) => {
                write!(formatter, "job.cancel authority is missing: {field}")
            }
        }
    }
}

impl std::error::Error for RepositoryExecutionSchedulerError {}

impl From<StorageError> for RepositoryExecutionSchedulerError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Production adapter over the one canonical product storage connection.
pub struct RepositoryExecutionScheduler<'storage> {
    storage: &'storage mut SqliteStorage,
}

/// Public Delivery cancellation command after the UI command envelope has
/// supplied its repository scope and request identity. The expected revision
/// is the Delivery revision; the queue revision is always read from the
/// durable `WorkRun` job below.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkRunCancellationRequest {
    pub scope: RepositorySchedulerScope,
    pub delivery_id: DeliveryId,
    pub work_run_id: WorkRunId,
    pub request_id: winwincode_domain::RequestId,
    pub expected_delivery_revision: u64,
    pub requested_at: Instant,
    /// Digest of the complete generated public command. Queue revision and
    /// server time are deliberately kept outside this value so retries can
    /// reconstruct the original queue request while freezing client input.
    pub public_command_digest: winwincode_domain::Sha256Digest,
}

impl<'storage> RepositoryExecutionScheduler<'storage> {
    #[must_use = "use the repository scheduler adapter"]
    pub const fn new(storage: &'storage mut SqliteStorage) -> Self {
        Self { storage }
    }

    /// Claims one fair job and returns only a typed command built from the
    /// committed queue/Registry receipt.
    ///
    /// # Errors
    ///
    /// Returns storage errors and rejects a non-canonical or mismatched sealed
    /// `ExecutionJob` before a command can leave the Control Plane.
    pub fn claim_next(
        &mut self,
        request: &RepositorySchedulerClaimRequest,
    ) -> Result<Option<JobDispatchMessage>, RepositoryExecutionSchedulerError> {
        let receipt = self.storage.repository_scheduler()?.claim_next(request)?;
        receipt
            .map(|receipt| {
                let replacement = self
                    .storage
                    .load_execution_scope_replacement(&receipt.job.job_id)?;
                dispatch_message(receipt, replacement.as_ref())
            })
            .transpose()
    }

    /// Claims one policy-eligible failed Job and returns the same typed
    /// dispatch used by ordinary and crash-replacement claims.
    ///
    /// # Errors
    ///
    /// Propagates receipt, retry-policy, queue, Registry, and sealed
    /// replacement-authority failures without constructing a fallback Job.
    pub fn retry_failed(
        &mut self,
        request: &RepositorySchedulerRetryRequest,
    ) -> Result<Option<JobDispatchMessage>, RepositoryExecutionSchedulerError> {
        let receipt = self.storage.repository_scheduler()?.retry_failed(request)?;
        receipt
            .map(|receipt| {
                let replacement = self
                    .storage
                    .load_execution_scope_replacement(&receipt.job.job_id)?;
                dispatch_message(receipt, replacement.as_ref())
            })
            .transpose()
    }

    /// Records one Worker dispatch result through the atomic repository
    /// scheduler seam and returns its durable Registry decision.
    ///
    /// # Errors
    ///
    /// Rejects malformed wire attempts and propagates scheduler authority or
    /// storage failures.
    pub fn record_dispatch_result(
        &mut self,
        repository_scope: &RepositoryScope,
        message: &JobDispatchResultMessage,
        server_time: &Instant,
    ) -> Result<RepositorySchedulerDispatchResultReceipt, RepositoryExecutionSchedulerError> {
        let attempt = u64::try_from(message.lease.attempt)
            .map_err(|_| RepositoryExecutionSchedulerError::InvalidExecutionJob("lease.attempt"))?;
        let error = message
            .error
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| RepositoryExecutionSchedulerError::InvalidExecutionJob("error"))?;
        let request = RepositorySchedulerDispatchResultRequest {
            scope: scheduler_scope(repository_scope),
            dispatch: DispatchResultRequest {
                checked_at: server_time.clone(),
                expires_at: message.lease.expires_at.clone(),
                fencing_token: message.lease.fencing_token.clone(),
                issued_at: message.lease.issued_at.clone(),
                job_id: message.job_id.clone(),
                lease_id: message.lease.lease_id.clone(),
                message_id: message.message_id.clone(),
                payload_digest: message.payload_digest.clone(),
                request_id: message.request_id.clone(),
                sent_at: message.sent_at.clone(),
                status: dispatch_status(&message.status),
                attempt,
                error,
                worker_id: message.lease.worker_id.clone(),
                worker_instance_id: message.lease.worker_instance_id.clone(),
                worker_session_id: message.worker_session_id.clone(),
            },
        };
        self.storage
            .repository_scheduler()?
            .record_dispatch_result(&request)
            .map_err(Into::into)
    }

    /// Resolves immutable repository scope from the durable queue before
    /// applying the same atomic dispatch-result path.
    ///
    /// # Errors
    ///
    /// Rejects a missing Job and propagates the same authority/storage errors
    /// as [`Self::record_dispatch_result`].
    pub fn record_dispatch_result_for_job(
        &mut self,
        message: &JobDispatchResultMessage,
        server_time: &Instant,
    ) -> Result<RepositorySchedulerDispatchResultReceipt, RepositoryExecutionSchedulerError> {
        let scope = self
            .storage
            .repository_scheduler()?
            .scope_for_job(&message.job_id)?;
        self.record_dispatch_result(&domain_scope(&scope), message, server_time)
    }

    /// Persists one user cancellation and returns an exact typed Worker
    /// command only after both dispatch and slot/session authority exist.
    ///
    /// # Errors
    ///
    /// Rejects missing/cross-job slot authority and storage failures.
    pub fn request_cancellation(
        &mut self,
        request: &RepositorySchedulerCancellationRequest,
    ) -> Result<Option<JobCancelMessage>, RepositoryExecutionSchedulerError> {
        let receipt = self
            .storage
            .repository_scheduler()?
            .request_cancellation(request)?;
        cancel_message(self.storage, &receipt)
    }

    /// Authorizes and persists cancellation for one canonical Delivery
    /// `WorkRun`. The caller supplies the Delivery revision, while the queue
    /// revision is obtained from the exact active `WorkRun` job so a client
    /// cannot cancel a different attempt by guessing queue state.
    ///
    /// # Errors
    ///
    /// Rejects a missing or stale Delivery, a `WorkRun` from another Delivery,
    /// a cross-repository job, and any queue/storage authority failure.
    pub fn request_cancellation_for_work_run(
        &mut self,
        request: &WorkRunCancellationRequest,
    ) -> Result<Option<JobCancelMessage>, RepositoryExecutionSchedulerError> {
        // Include terminal queue rows so an already committed cancellation can
        // be replayed after the Delivery and queue have moved on. The storage
        // run-aware method performs the final identity check inside its
        // immediate transaction, closing replacement races.
        let record = self
            .storage
            .repository_scheduler()?
            .list_jobs(&request.scope, &[])?
            .into_iter()
            .find(|record| record.work_run_id.as_ref() == Some(&request.work_run_id))
            .ok_or_else(|| StorageError::invalid_input("WorkRun job is missing"))?;
        if record.scope.delivery_id.as_ref() != Some(&request.delivery_id) {
            return Err(StorageError::invalid_input(
                "WorkRun job does not belong to the requested Delivery",
            )
            .into());
        }
        let replay = record
            .cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.request_id == request.request_id);
        if !replay {
            let delivery_stream = format!("delivery:{}", request.delivery_id.0);
            let state = self
                .storage
                .load_state(&delivery_stream)?
                .ok_or_else(|| StorageError::invalid_input("Delivery state is missing"))?;
            if state.revision != request.expected_delivery_revision {
                return Err(RepositoryExecutionSchedulerError::StaleDeliveryRevision);
            }
            let delivery = Delivery::decode_json(&state.payload)
                .map_err(|error| StorageError::invalid_input(error.to_string()))?;
            if delivery.id() != &request.delivery_id || delivery.revision() != state.revision {
                return Err(
                    StorageError::invalid_input("Delivery state identity is inconsistent").into(),
                );
            }
            if record.state == ExecutionJobState::Queued {
                validate_queued_workrun_job(&record, &delivery, request)?;
            } else {
                let run = delivery
                    .snapshot()
                    .work_run_aggregate
                    .runs
                    .iter()
                    .find(|run| run.id == request.work_run_id)
                    .ok_or_else(|| {
                        StorageError::invalid_input("WorkRun is missing from Delivery")
                    })?;
                if !matches!(
                    run.state,
                    winwincode_domain::WorkRunState::Leased
                        | winwincode_domain::WorkRunState::Running
                ) {
                    return Err(StorageError::invalid_input("WorkRun is not cancellable").into());
                }
                if run.execution_job_id != record.job_id {
                    return Err(StorageError::invalid_input(
                        "WorkRun job does not match the Delivery aggregate",
                    )
                    .into());
                }
            }
        }
        let (expected_revision, requested_at) = match record.cancellation.as_ref() {
            Some(cancellation) => (
                record.revision.checked_sub(1).ok_or_else(|| {
                    StorageError::invalid_input("cancellation revision is invalid")
                })?,
                cancellation.requested_at.clone(),
            ),
            None => (record.revision, request.requested_at.clone()),
        };
        let receipt = self
            .storage
            .repository_scheduler()?
            .request_cancellation_for_work_run_with_command_digest(
                &RepositorySchedulerCancellationRequest {
                    scope: request.scope.clone(),
                    job_id: record.job_id,
                    request_id: request.request_id.clone(),
                    expected_revision,
                    requested_at,
                },
                &request.work_run_id,
                &request.public_command_digest,
            )?;
        cancel_message(self.storage, &receipt)
    }

    /// Rebuilds every outstanding exact `job.cancel` command after restart.
    ///
    /// # Errors
    ///
    /// Rejects a corrupt cancellation revision or missing canonical
    /// `WorkerSession` slot rather than fabricating session identity.
    pub fn pending_cancellations(
        &mut self,
        scope: &RepositorySchedulerScope,
    ) -> Result<Vec<JobCancelMessage>, RepositoryExecutionSchedulerError> {
        let jobs = self
            .storage
            .repository_scheduler()?
            .list_jobs(scope, &[ExecutionJobState::Cancelling])?;
        let mut messages = Vec::with_capacity(jobs.len());
        for job in jobs {
            let cancellation = job.cancellation.as_ref().ok_or(
                RepositoryExecutionSchedulerError::MissingCancellationAuthority(
                    "cancellation receipt",
                ),
            )?;
            let expected_revision = job.revision.checked_sub(1).ok_or(
                RepositoryExecutionSchedulerError::MissingCancellationAuthority("queue revision"),
            )?;
            let request = RepositorySchedulerCancellationRequest {
                scope: scope.clone(),
                job_id: job.job_id,
                request_id: cancellation.request_id.clone(),
                expected_revision,
                requested_at: cancellation.requested_at.clone(),
            };
            let receipt = if let Some(work_run_id) = job.work_run_id.as_ref() {
                let mut scheduler = self.storage.repository_scheduler()?;
                match scheduler.replay_cancellation_for_work_run(&request, work_run_id)? {
                    Some(receipt) => receipt,
                    None => scheduler.request_cancellation_for_work_run(&request, work_run_id)?,
                }
            } else {
                self.storage
                    .repository_scheduler()?
                    .request_cancellation(&request)?
            };
            if let Some(message) = cancel_message(self.storage, &receipt)? {
                messages.push(message);
            }
        }
        Ok(messages)
    }

    /// Commits queue and Registry terminal authority in one storage
    /// transaction.
    ///
    /// # Errors
    ///
    /// Propagates exact replay, fence, revision, and storage failures.
    pub fn settle_terminal(
        &mut self,
        request: &RepositorySchedulerTerminalRequest,
    ) -> Result<RepositorySchedulerTerminalReceipt, RepositoryExecutionSchedulerError> {
        self.storage
            .repository_scheduler()?
            .settle_terminal(request)
            .map_err(Into::into)
    }
}

fn validate_queued_workrun_job(
    record: &ExecutionJobRecord,
    delivery: &Delivery,
    request: &WorkRunCancellationRequest,
) -> Result<(), RepositoryExecutionSchedulerError> {
    let job = decode_execution_job(record)?;
    crate::delivery_execution::validate_workrun_execution_job(&job)
        .map_err(|_| RepositoryExecutionSchedulerError::InvalidExecutionJob("WorkRun fields"))?;
    let ExecutionScope::WorkRunExecutionScope(scope) = &job.scope else {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "scope",
        ));
    };
    if scope.work_run_id != request.work_run_id
        || i64::try_from(record.attempt).ok() != Some(scope.attempt)
    {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "WorkRun identity or attempt",
        ));
    }
    let input =
        job.work_input
            .as_ref()
            .ok_or(RepositoryExecutionSchedulerError::InvalidExecutionJob(
                "workInput",
            ))?;
    let aggregate = &delivery.snapshot().work_run_aggregate;
    if input.work_contract != aggregate.contract
        || scope.work_contract_id != aggregate.contract.id
        || scope.work_contract_revision != aggregate.contract.revision
    {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "contract",
        ));
    }
    let item = aggregate
        .items
        .iter()
        .find(|item| item.id == scope.work_item_id)
        .ok_or(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "workItem",
        ))?;
    if item.state != WorkItemState::Ready || scope.work_item_revision != item.revision {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "workItem revision",
        ));
    }
    // Depending on the producer path, the immutable dispatch may carry either
    // the source Ready item or its InProgress execution copy while the
    // Delivery aggregate remains Ready until accepted dispatch appends the
    // canonical WorkRun. Compare every other item field and normalize only
    // this intentional pre-acceptance state transition.
    let mut input_item = input.work_item.clone();
    if !matches!(
        input_item.state,
        WorkItemState::Ready | WorkItemState::InProgress
    ) {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "workItem state",
        ));
    }
    input_item.state = item.state.clone();
    if input_item != *item {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "workItem",
        ));
    }
    Ok(())
}

fn dispatch_message(
    receipt: winwincode_storage::RepositorySchedulerClaimReceipt,
    replacement: Option<&ExecutionScopeReplacementAuthority>,
) -> Result<JobDispatchMessage, RepositoryExecutionSchedulerError> {
    let job = decode_execution_job(&receipt.job)?;
    let lease = lease_stamp(&receipt.lease)?;
    let replacement_authority = replacement
        .filter(|authority| authority.replacement_attempt() == receipt.lease.attempt)
        .map(|authority| replacement_message(authority, &job, &lease))
        .transpose()?;
    Ok(JobDispatchMessage {
        job,
        kind: JobDispatchMessageKind::JobDispatch,
        lease,
        message_id: receipt.message_id,
        replacement_authority,
        request_id: receipt.request_id,
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: receipt.lease.issued_at,
    })
}

fn replacement_message(
    authority: &ExecutionScopeReplacementAuthority,
    job: &ExecutionJob,
    successor_lease: &ExecutionLeaseStamp,
) -> Result<ExecutionJobReplacementAuthority, RepositoryExecutionSchedulerError> {
    if authority.job_id() != &job.job_id
        || authority.replacement_lease().job_id != successor_lease.job_id
        || authority.replacement_lease().attempt
            != u64::try_from(successor_lease.attempt).unwrap_or_default()
        || !scope_matches_replacement(authority, &job.scope)
    {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "replacementAuthority",
        ));
    }
    let predecessor_session_identity = match (
        authority.previous_worker_session_id(),
        authority.predecessor_slot(),
    ) {
        (None | Some(_), None) => None,
        (Some(worker_session_id), Some(slot)) if worker_session_id == &slot.worker_session_id => {
            Some(SessionIdentity {
                codex_thread_id: slot.codex_thread_id.clone(),
                product_session_id: authority.scope().product_session_id.clone(),
                work_run_id: authority.predecessor_work_run_id().cloned(),
                worker_session_id: slot.worker_session_id.clone(),
            })
        }
        _ => {
            return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
                "replacementAuthority.predecessorSessionIdentity",
            ));
        }
    };
    Ok(ExecutionJobReplacementAuthority {
        created_at: authority.created_at().clone(),
        logical_job_digest: authority.logical_job_digest().clone(),
        predecessor_lease: lease_stamp(authority.predecessor_lease())?,
        predecessor_session_identity,
        receipt_digest: authority.receipt_digest().clone(),
        receipt_id: authority.receipt_id().clone(),
        scope: job.scope.clone(),
        successor_lease: successor_lease.clone(),
    })
}

fn scope_matches_replacement(
    authority: &ExecutionScopeReplacementAuthority,
    scope: &ExecutionScope,
) -> bool {
    match scope {
        ExecutionScope::ProductSessionExecutionScope(scope) => {
            scope.product_session_id == authority.scope().product_session_id
                && authority.scope().delivery_id.is_none()
                && authority.work_run_id().is_none()
        }
        ExecutionScope::WorkRunExecutionScope(scope) => {
            scope.product_session_id == authority.scope().product_session_id
                && authority.scope().delivery_id.is_some()
                && Some(&scope.work_run_id) == authority.work_run_id()
        }
    }
}

fn decode_execution_job(
    record: &ExecutionJobRecord,
) -> Result<ExecutionJob, RepositoryExecutionSchedulerError> {
    let job: ExecutionJob = serde_json::from_slice(&record.dispatch_payload)
        .map_err(|_| RepositoryExecutionSchedulerError::InvalidExecutionJob("dispatchPayload"))?;
    let canonical = serde_json::to_vec(&job)
        .map_err(|_| RepositoryExecutionSchedulerError::InvalidExecutionJob("canonical JSON"))?;
    if canonical != record.dispatch_payload {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "non-canonical dispatchPayload",
        ));
    }
    if job.job_id != record.job_id {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "jobId",
        ));
    }
    if job.payload_digest != record.payload_digest {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "payloadDigest",
        ));
    }
    if u64::try_from(job.attempt).ok() != Some(record.attempt) {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "attempt",
        ));
    }
    if job.workspace.repository_id != record.scope.repository_id {
        return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
            "workspace.repositoryId",
        ));
    }
    match &job.scope {
        ExecutionScope::ProductSessionExecutionScope(scope)
            if scope.product_session_id == record.scope.product_session_id
                && record.scope.delivery_id.is_none()
                && record.work_run_id.is_none() => {}
        ExecutionScope::WorkRunExecutionScope(scope)
            if scope.product_session_id == record.scope.product_session_id
                && record.scope.delivery_id.is_some()
                && Some(&scope.work_run_id) == record.work_run_id.as_ref() => {}
        _ => {
            return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
                "scope",
            ));
        }
    }
    Ok(job)
}

fn cancel_message(
    _storage: &mut SqliteStorage,
    receipt: &RepositorySchedulerCancellationReceipt,
) -> Result<Option<JobCancelMessage>, RepositoryExecutionSchedulerError> {
    let (Some(lease), Some(worker_session_id), Some(message_id)) = (
        receipt.lease.as_ref(),
        receipt.worker_session_id.as_ref(),
        receipt.message_id.as_ref(),
    ) else {
        if receipt.lease.is_none()
            && receipt.worker_session_id.is_none()
            && receipt.message_id.is_none()
        {
            return Ok(None);
        }
        return Err(
            RepositoryExecutionSchedulerError::MissingCancellationAuthority(
                "partial scheduler receipt",
            ),
        );
    };
    let cancellation = receipt.job.cancellation.as_ref().ok_or(
        RepositoryExecutionSchedulerError::MissingCancellationAuthority("cancellation receipt"),
    )?;
    // An accepted dispatch may be cancelled before its WorkerSession binds a
    // slot. Keep the queue receipt durable and let restart/pending-cancel
    // orchestration emit the command once codex thread authority exists.
    let Some(codex_thread_id) = receipt.codex_thread_id.clone() else {
        return Ok(None);
    };
    Ok(Some(JobCancelMessage {
        kind: JobCancelMessageKind::JobCancel,
        lease: lease_stamp(lease)?,
        message_id: message_id.clone(),
        reason: JobCancelMessageReason::UserRequested,
        requested_at: cancellation.requested_at.clone(),
        request_id: receipt.request_id.clone(),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: cancellation.requested_at.clone(),
        session_identity: SessionIdentity {
            codex_thread_id,
            product_session_id: receipt.job.scope.product_session_id.clone(),
            work_run_id: receipt.job.work_run_id.clone(),
            worker_session_id: worker_session_id.clone(),
        },
        worker_session_id: worker_session_id.clone(),
    }))
}

fn lease_stamp(
    lease: &ExecutionLeaseRecord,
) -> Result<ExecutionLeaseStamp, RepositoryExecutionSchedulerError> {
    Ok(ExecutionLeaseStamp {
        attempt: i64::try_from(lease.attempt)
            .map_err(|_| RepositoryExecutionSchedulerError::InvalidExecutionJob("attempt"))?,
        expires_at: lease.expires_at.clone(),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
    })
}

fn scheduler_scope(scope: &RepositoryScope) -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

fn domain_scope(scope: &RepositorySchedulerScope) -> RepositoryScope {
    RepositoryScope {
        kind: winwincode_domain::RepositoryScopeKind::Repository,
        organization_id: scope.organization_id.clone(),
        workspace_id: scope.workspace_id.clone(),
        project_id: scope.project_id.clone(),
        repository_id: scope.repository_id.clone(),
    }
}

const fn dispatch_status(status: &JobDispatchResultMessageStatus) -> DispatchResultStatus {
    match status {
        JobDispatchResultMessageStatus::Accepted => DispatchResultStatus::Accepted,
        JobDispatchResultMessageStatus::Duplicate => DispatchResultStatus::Duplicate,
        JobDispatchResultMessageStatus::Conflict => DispatchResultStatus::Conflict,
        JobDispatchResultMessageStatus::RejectedCapacity => DispatchResultStatus::RejectedCapacity,
        JobDispatchResultMessageStatus::RejectedCapability => {
            DispatchResultStatus::RejectedCapability
        }
        JobDispatchResultMessageStatus::RejectedExpiredLease => {
            DispatchResultStatus::RejectedExpiredLease
        }
        JobDispatchResultMessageStatus::RejectedStaleFencingToken => {
            DispatchResultStatus::RejectedStaleFencingToken
        }
        JobDispatchResultMessageStatus::RejectedWorkerInstance => {
            DispatchResultStatus::RejectedWorkerInstance
        }
    }
}
