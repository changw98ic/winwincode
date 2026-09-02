// SPDX-License-Identifier: Apache-2.0

//! Typed Control Plane adapter for the repository-scoped durable scheduler.
//!
//! Storage owns selection, receipts, queue state, and Registry authority. This
//! module only decodes the already sealed `ExecutionJob` bytes and constructs
//! generated `ExecutionPort` commands from committed authority.

use std::fmt;

use winwincode_domain::{Instant, RepositoryScope, SchemaVersion, SessionIdentity};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionJobReplacementAuthority, ExecutionLeaseStamp, ExecutionScope,
    JobCancelMessage, JobCancelMessageKind, JobCancelMessageReason, JobDispatchMessage,
    JobDispatchMessageKind, JobDispatchResultMessage, JobDispatchResultMessageStatus,
};
use winwincode_storage::{
    DispatchResultRequest, DispatchResultStatus, ExecutionJobRecord, ExecutionJobState,
    ExecutionLeaseRecord, ExecutionScopeReplacementAuthority,
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
    InvalidExecutionJob(&'static str),
    MissingCancellationAuthority(&'static str),
}

impl fmt::Display for RepositoryExecutionSchedulerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => {
                write!(formatter, "repository scheduler storage error: {error}")
            }
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
            let receipt = self.storage.repository_scheduler()?.request_cancellation(
                &RepositorySchedulerCancellationRequest {
                    scope: scope.clone(),
                    job_id: job.job_id,
                    request_id: cancellation.request_id.clone(),
                    expected_revision,
                    requested_at: cancellation.requested_at.clone(),
                },
            )?;
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
                stage_run_id: authority.stage_run_id().cloned(),
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
                && authority.stage_run_id().is_none()
        }
        ExecutionScope::DeliveryStageExecutionScope(scope) => {
            scope.product_session_id == authority.scope().product_session_id
                && Some(&scope.delivery_id) == authority.scope().delivery_id.as_ref()
                && Some(&scope.stage_run_id) == authority.stage_run_id()
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
                && record.stage_run_id.is_none() => {}
        ExecutionScope::DeliveryStageExecutionScope(scope)
            if scope.product_session_id == record.scope.product_session_id
                && Some(&scope.delivery_id) == record.scope.delivery_id.as_ref()
                && Some(&scope.stage_run_id) == record.stage_run_id.as_ref() => {}
        _ => {
            return Err(RepositoryExecutionSchedulerError::InvalidExecutionJob(
                "scope",
            ));
        }
    }
    Ok(job)
}

fn cancel_message(
    storage: &mut SqliteStorage,
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
    let slot = storage
        .worker_session_slots()
        .map_err(|error| StorageError::adapter(format!("Worker slot cannot be opened: {error}")))?
        .load(worker_session_id)
        .map_err(|error| StorageError::adapter(format!("Worker slot cannot be read: {error}")))?
        .ok_or(
            RepositoryExecutionSchedulerError::MissingCancellationAuthority("WorkerSession slot"),
        )?;
    if slot.authority.job_id != receipt.job.job_id
        || slot.authority.worker_session_id != *worker_session_id
        || slot.authority.worker_id != lease.worker_id
        || slot.authority.worker_instance_id != lease.worker_instance_id
        || slot.authority.lease_id != lease.lease_id
        || slot.authority.attempt != lease.attempt
        || slot.authority.fencing_token != lease.fencing_token
    {
        return Err(
            RepositoryExecutionSchedulerError::MissingCancellationAuthority(
                "WorkerSession slot identity",
            ),
        );
    }
    let cancellation = receipt.job.cancellation.as_ref().ok_or(
        RepositoryExecutionSchedulerError::MissingCancellationAuthority("cancellation receipt"),
    )?;
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
            codex_thread_id: slot.authority.codex_thread_id,
            product_session_id: receipt.job.scope.product_session_id.clone(),
            stage_run_id: receipt.job.stage_run_id.clone(),
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
