// SPDX-License-Identifier: Apache-2.0

//! Admission inputs derived from a sealed Job and the caller's authenticated target.

use std::fmt;
use winwincode_domain::{RequestId, UserId};
use winwincode_execution_port::generated::{
    ExecutionJob, ExecutionLimits, ExecutionWorkspaceWriteMode,
};
use winwincode_storage::{
    ExecutionAdmissionBoundary, ExecutionAdmissionErrorCode, ExecutionJobRecord,
    ExecutionRepositoryAccess, ExecutionReservationRequest, WorkerPoolId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionAdmissionInputError {
    InvalidPayload,
    IdentityMismatch,
    InvalidRuntimePolicy,
}

impl fmt::Display for ExecutionAdmissionInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidPayload => "sealed execution admission payload is invalid",
            Self::IdentityMismatch => "sealed execution admission identity differs",
            Self::InvalidRuntimePolicy => "sealed execution runtime policy is invalid",
        })
    }
}

impl std::error::Error for ExecutionAdmissionInputError {}

/// Decodes the durable sealed input and validates its queue identity and runtime policy.
/// # Errors
/// Rejects changed Job identities and inconsistent or out-of-range runtime limits.
pub fn execution_admission_job(
    record: &ExecutionJobRecord,
) -> Result<(ExecutionJob, Option<u64>), ExecutionAdmissionInputError> {
    let job: ExecutionJob = serde_json::from_slice(&record.dispatch_payload)
        .map_err(|_| ExecutionAdmissionInputError::InvalidPayload)?;
    validate_job_identity(record, &job)?;
    let runtime = runtime_limit_millis(&job.limits)?;
    Ok((job, runtime))
}

fn validate_job_identity(
    record: &ExecutionJobRecord,
    job: &ExecutionJob,
) -> Result<(), ExecutionAdmissionInputError> {
    if job.job_id != record.job_id
        || job.payload_digest != record.payload_digest
        || u64::try_from(job.attempt).ok() != Some(record.attempt)
    {
        return Err(ExecutionAdmissionInputError::IdentityMismatch);
    }
    Ok(())
}

fn runtime_limit_millis(
    limits: &ExecutionLimits,
) -> Result<Option<u64>, ExecutionAdmissionInputError> {
    if limits.deadline_at.is_some() != limits.max_runtime_seconds.is_some() {
        return Err(ExecutionAdmissionInputError::InvalidRuntimePolicy);
    }
    limits
        .max_runtime_seconds
        .map(|seconds| {
            u64::try_from(seconds)
                .ok()
                .filter(|seconds| (1..=604_800).contains(seconds))
                .and_then(|seconds| seconds.checked_mul(1_000))
                .ok_or(ExecutionAdmissionInputError::InvalidRuntimePolicy)
        })
        .transpose()
}

/// Builds the shared request. Scope and repository writes come only from the sealed Job.
/// The caller supplies its already-authenticated user/pool and original operation identity.
/// # Errors
/// Rejects a Job that differs from durable input or has an invalid runtime policy.
pub fn execution_reservation_request(
    record: &ExecutionJobRecord,
    job: &ExecutionJob,
    user_id: UserId,
    worker_pool_id: WorkerPoolId,
    request_id: RequestId,
) -> Result<ExecutionReservationRequest, ExecutionAdmissionInputError> {
    let (sealed, runtime_limit_millis) = execution_admission_job(record)?;
    if sealed != *job {
        return Err(ExecutionAdmissionInputError::IdentityMismatch);
    }
    Ok(ExecutionReservationRequest {
        scope: record.scope.clone(),
        user_id,
        worker_pool_id,
        job_id: record.job_id.clone(),
        request_id,
        repository_access: match job.workspace.write_mode {
            ExecutionWorkspaceWriteMode::ReadOnly => ExecutionRepositoryAccess::ReadOnly,
            ExecutionWorkspaceWriteMode::Candidate => ExecutionRepositoryAccess::IsolatedWrite {
                worktree_key: format!("job-{}", record.job_id.0),
            },
        },
        reserved_tokens: None,
        reserved_cost_microunits: None,
        runtime_limit_millis,
        submitted_at: record.submitted_at.clone(),
    })
}

#[must_use]
pub fn execution_admission_boundaries(
    scope: &winwincode_storage::ExecutionQueueScope,
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
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: worker_pool_id.clone(),
        },
    ];
    if let Some(delivery_id) = &scope.delivery_id {
        boundaries.push(ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: delivery_id.clone(),
        });
    }
    boundaries
}

#[must_use]
pub const fn execution_admission_deferred(code: ExecutionAdmissionErrorCode) -> bool {
    matches!(
        code,
        ExecutionAdmissionErrorCode::RevisionConflict
            | ExecutionAdmissionErrorCode::QueueCapacityExhausted
            | ExecutionAdmissionErrorCode::ConcurrencyExhausted
            | ExecutionAdmissionErrorCode::TokenBudgetExhausted
            | ExecutionAdmissionErrorCode::CostBudgetExhausted
            | ExecutionAdmissionErrorCode::RepositoryWriteConflict
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_domain::Instant;

    #[test]
    fn uncapped_runtime_stays_uncapped_and_partial_limits_are_rejected() {
        let mut limits = ExecutionLimits {
            deadline_at: None,
            max_runtime_seconds: None,
            max_artifact_bytes: 1,
        };
        assert_eq!(runtime_limit_millis(&limits).unwrap(), None);
        limits.deadline_at = Some(Instant("2030-01-01T00:00:00.000Z".into()));
        assert_eq!(
            runtime_limit_millis(&limits),
            Err(ExecutionAdmissionInputError::InvalidRuntimePolicy)
        );
        limits.max_runtime_seconds = Some(604_800);
        assert_eq!(runtime_limit_millis(&limits).unwrap(), Some(604_800_000));
        for seconds in [-1, 0, 604_801, i64::MAX] {
            limits.max_runtime_seconds = Some(seconds);
            assert_eq!(
                runtime_limit_millis(&limits),
                Err(ExecutionAdmissionInputError::InvalidRuntimePolicy)
            );
        }
    }
}
