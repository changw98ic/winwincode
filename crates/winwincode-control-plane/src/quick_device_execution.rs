use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, ProductSessionId, RequestId, UserId};
use winwincode_execution_port::generated::{ExecutionJob, ExecutionWorkspaceWriteMode};
use winwincode_storage::{
    DeviceExecutionBindingIssuance, DeviceExecutionBindingRecord, DeviceExecutionBindingState,
    DeviceExecutionFactsAttachment, DeviceExecutionReservationFacts, ExecutionAdmissionBoundary,
    ExecutionAdmissionErrorCode, ExecutionJobRecord, ExecutionQueueScope,
    ExecutionRepositoryAccess, ExecutionReservationRequest, ExecutionReservationState,
    ProductStateStorage, SqliteStorage, WorkerPoolId,
};

use crate::client_launch_grant::{
    LaunchGrantState, WorkerLaunchGrantRecord, WorkerLaunchGrantService,
};
use crate::device_execution_binding::DeviceExecutionBindingService;
use crate::product_session_service::chat_turn_execution_job_id;

pub const QUICK_DEVICE_WORKER_POOL_ID: &str = "wpl_000000000000000000000000D3";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuickDeviceDispatchErrorKind {
    InvalidInput,
    AnchorNotLive,
    WorkerSessionEnded,
    DispatchConflict,
    AdmissionUnavailable,
    CorruptState,
    Storage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickDeviceDispatchError {
    kind: QuickDeviceDispatchErrorKind,
    message: String,
}

impl QuickDeviceDispatchError {
    #[must_use]
    pub const fn kind(&self) -> QuickDeviceDispatchErrorKind {
        self.kind
    }

    fn new(kind: QuickDeviceDispatchErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(QuickDeviceDispatchErrorKind::InvalidInput, message)
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::new(QuickDeviceDispatchErrorKind::CorruptState, message)
    }

    fn storage() -> Self {
        Self::new(
            QuickDeviceDispatchErrorKind::Storage,
            "Quick device dispatch storage failed",
        )
    }
}

impl fmt::Display for QuickDeviceDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for QuickDeviceDispatchError {}

#[derive(Clone, Debug, PartialEq)]
pub struct QuickDeviceDispatch {
    pub binding: DeviceExecutionBindingRecord,
    pub facts: DeviceExecutionReservationFacts,
}

pub fn dispatch_turn_to_device_worker(
    storage: &mut SqliteStorage,
    product_session_id: &ProductSessionId,
    turn_request_id: &RequestId,
    now: &Instant,
) -> Result<Option<QuickDeviceDispatch>, QuickDeviceDispatchError> {
    let execution_job_id = chat_turn_execution_job_id(turn_request_id, product_session_id);
    let existing = DeviceExecutionBindingService::new(storage)
        .facts(&execution_job_id.0)
        .map_err(|_| QuickDeviceDispatchError::storage())?;
    let anchor = if let Some(facts) = existing {
        let ledger = storage
            .worker_launch_grant_ledger()
            .map_err(|_| QuickDeviceDispatchError::storage())?;
        ledger
            .recovery_launch_for_grant(&facts.worker_launch_grant_id)
            .map_err(|_| QuickDeviceDispatchError::storage())?
            .or(ledger
                .snapshot(&facts.worker_launch_grant_id)
                .map_err(|_| QuickDeviceDispatchError::storage())?)
    } else {
        WorkerLaunchGrantService::new(storage)
            .newest_grant_for_product_session(&product_session_id.0)
            .map_err(|_| QuickDeviceDispatchError::storage())?
    };
    let Some(anchor) = anchor else {
        return Ok(None);
    };
    dispatch_chat_job_to_device_worker(storage, &execution_job_id, &anchor, now).map(Some)
}

pub fn dispatch_chat_job_to_device_worker(
    storage: &mut SqliteStorage,
    execution_job_id: &ExecutionJobId,
    anchor: &WorkerLaunchGrantRecord,
    now: &Instant,
) -> Result<QuickDeviceDispatch, QuickDeviceDispatchError> {
    let record = storage
        .load_execution_job_record(execution_job_id)
        .map_err(|_| QuickDeviceDispatchError::storage())?
        .ok_or_else(|| {
            QuickDeviceDispatchError::invalid_input("the Chat execution job does not exist")
        })?;
    if record.work_run_id.is_some()
        || record.cancellation.is_some()
        || anchor.work_run_id.is_some()
        || anchor.product_session_id.as_deref() != Some(&record.scope.product_session_id.0)
    {
        return Err(QuickDeviceDispatchError::invalid_input(
            "the Chat execution scope differs from its launch",
        ));
    }
    if !matches!(
        anchor.state,
        LaunchGrantState::Issued | LaunchGrantState::Consumed
    ) {
        return Err(QuickDeviceDispatchError::new(
            QuickDeviceDispatchErrorKind::AnchorNotLive,
            "the session's launch anchor grant can no longer execute work",
        ));
    }

    let binding = {
        let mut bindings = DeviceExecutionBindingService::new(storage);
        if let Some(existing) = bindings
            .snapshot(&anchor.worker_session_id)
            .map_err(|_| QuickDeviceDispatchError::storage())?
        {
            if existing.state != DeviceExecutionBindingState::Bound {
                return Err(QuickDeviceDispatchError::new(
                    QuickDeviceDispatchErrorKind::WorkerSessionEnded,
                    "the anchored device worker session already released its binding",
                ));
            }
            existing
        } else {
            let command = bind_command(anchor)?;
            bindings
                .bind(&command, now)
                .map_err(|_| QuickDeviceDispatchError::storage())?
                .binding
        }
    };
    {
        let mut bindings = DeviceExecutionBindingService::new(storage);
        if let Some(existing) = bindings
            .facts(execution_job_id.0.as_str())
            .map_err(|_| QuickDeviceDispatchError::storage())?
        {
            if existing.worker_launch_grant_id == anchor.worker_launch_grant_id {
                if existing.role.is_some() || existing.work_run_id.is_some() {
                    return Err(QuickDeviceDispatchError::invalid_input(
                        "the Chat route carries a WorkRun role",
                    ));
                }
                return Ok(QuickDeviceDispatch {
                    binding,
                    facts: existing,
                });
            }
            let facts = winwincode_storage::stage_device_execution_recovery(
                storage,
                execution_job_id,
                &anchor.worker_launch_grant_id,
                None,
                now,
            )
            .map_err(|_| {
                QuickDeviceDispatchError::new(
                    QuickDeviceDispatchErrorKind::DispatchConflict,
                    "the Chat replacement has no sealed Device recovery authority",
                )
            })?;
            return Ok(QuickDeviceDispatch { binding, facts });
        }
    }

    ensure_device_admission_reservation(storage, execution_job_id, anchor)?;

    let facts = {
        let mut bindings = DeviceExecutionBindingService::new(storage);
        let attachment = DeviceExecutionFactsAttachment::try_new(
            derived_id("req_", FACTS_REQUEST_NAMESPACE, &execution_job_id.0),
            execution_job_id.0.as_str(),
            anchor.worker_launch_grant_id.as_str(),
        )
        .map_err(|error| QuickDeviceDispatchError::invalid_input(error.to_string()))?;
        bindings
            .attach_facts(&attachment, now)
            .map_err(|_| QuickDeviceDispatchError::storage())?
            .facts
    };
    Ok(QuickDeviceDispatch { binding, facts })
}

fn bind_command(
    anchor: &WorkerLaunchGrantRecord,
) -> Result<DeviceExecutionBindingIssuance, QuickDeviceDispatchError> {
    let binding_id = derived_id("deb_", BINDING_ID_NAMESPACE, &anchor.worker_launch_grant_id);
    let request_id = derived_id(
        "req_",
        BIND_REQUEST_NAMESPACE,
        &anchor.worker_launch_grant_id,
    );
    DeviceExecutionBindingIssuance::try_new(
        binding_id,
        request_id,
        anchor.worker_launch_grant_id.clone(),
        anchor.client_node_id.clone(),
        anchor.client_instance_id.clone(),
        anchor.holder_user_id.clone(),
        anchor.occupancy_lease_id.clone(),
        anchor.occupancy_fencing_token,
        anchor.repository_binding_id.clone(),
        anchor.worker_session_id.clone(),
        anchor.product_session_id.clone(),
        anchor.work_run_id.as_ref().map(|id| id.0.clone()),
    )
    .map_err(|error| QuickDeviceDispatchError::invalid_input(error.to_string()))
}

fn ensure_device_admission_reservation(
    storage: &mut SqliteStorage,
    execution_job_id: &ExecutionJobId,
    anchor: &WorkerLaunchGrantRecord,
) -> Result<(), QuickDeviceDispatchError> {
    let record = load_queued_job(storage, execution_job_id)?;
    let job = decode_execution_job(&record)?;
    let runtime_limit_millis = job_runtime_limit_millis(&job)?;
    let mut admission = storage
        .execution_admission()
        .map_err(|_| QuickDeviceDispatchError::storage())?;
    for boundary in admission_boundaries(&record.scope) {
        admission
            .configure_runtime_policy(boundary, runtime_limit_millis.is_some())
            .map_err(|error| admission_error(&error))?;
    }
    if let Some(existing) = admission
        .load_reservation_by_job(execution_job_id)
        .map_err(|_| QuickDeviceDispatchError::storage())?
    {
        return match existing.state {
            ExecutionReservationState::Queued | ExecutionReservationState::Running => Ok(()),
            ExecutionReservationState::Released | ExecutionReservationState::Settled => {
                Err(QuickDeviceDispatchError::new(
                    QuickDeviceDispatchErrorKind::DispatchConflict,
                    "the turn's execution reservation is already terminal",
                ))
            }
        };
    }
    let request = ExecutionReservationRequest {
        scope: record.scope.clone(),
        user_id: UserId(anchor.holder_user_id.clone()),
        worker_pool_id: WorkerPoolId(QUICK_DEVICE_WORKER_POOL_ID.to_owned()),
        job_id: record.job_id.clone(),
        request_id: RequestId(derived_id(
            "req_",
            RESERVATION_REQUEST_NAMESPACE,
            &record.job_id.0,
        )),
        repository_access: repository_access(&job, &record.job_id),
        reserved_tokens: None,
        reserved_cost_microunits: None,
        runtime_limit_millis,
        submitted_at: record.submitted_at.clone(),
    };
    admission
        .reserve(&request)
        .map(|_| ())
        .map_err(|error| admission_error(&error))
}

fn load_queued_job(
    storage: &mut SqliteStorage,
    execution_job_id: &ExecutionJobId,
) -> Result<ExecutionJobRecord, QuickDeviceDispatchError> {
    let record = storage
        .load_execution_job_record(execution_job_id)
        .map_err(|_| QuickDeviceDispatchError::storage())?
        .ok_or_else(|| {
            QuickDeviceDispatchError::corrupt("the committed Chat turn has no queued job")
        })?;
    if record.job_id != *execution_job_id {
        return Err(QuickDeviceDispatchError::corrupt(
            "the queued job identity does not match the turn",
        ));
    }
    Ok(record)
}

fn decode_execution_job(
    record: &ExecutionJobRecord,
) -> Result<ExecutionJob, QuickDeviceDispatchError> {
    let job: ExecutionJob = serde_json::from_slice(&record.dispatch_payload).map_err(|_| {
        QuickDeviceDispatchError::corrupt("the queued dispatch payload is not an ExecutionJob")
    })?;
    if job.job_id != record.job_id
        || job.payload_digest != record.payload_digest
        || job.attempt != i64::try_from(record.attempt).unwrap_or(-1)
    {
        return Err(QuickDeviceDispatchError::corrupt(
            "the queued dispatch payload does not match its durable job",
        ));
    }
    Ok(job)
}

fn job_runtime_limit_millis(job: &ExecutionJob) -> Result<Option<u64>, QuickDeviceDispatchError> {
    if job.limits.deadline_at.is_some() != job.limits.max_runtime_seconds.is_some() {
        return Err(QuickDeviceDispatchError::corrupt(
            "the queued job runtime policy is inconsistent",
        ));
    }
    job.limits
        .max_runtime_seconds
        .map(|seconds| {
            u64::try_from(seconds)
                .ok()
                .filter(|seconds| (1..=604_800).contains(seconds))
                .and_then(|seconds| seconds.checked_mul(1_000))
                .ok_or_else(|| {
                    QuickDeviceDispatchError::corrupt(
                        "the queued job execution deadline is invalid",
                    )
                })
        })
        .transpose()
}

fn repository_access(job: &ExecutionJob, job_id: &ExecutionJobId) -> ExecutionRepositoryAccess {
    match job.workspace.write_mode {
        ExecutionWorkspaceWriteMode::ReadOnly => ExecutionRepositoryAccess::ReadOnly,
        ExecutionWorkspaceWriteMode::Candidate => ExecutionRepositoryAccess::IsolatedWrite {
            worktree_key: format!("job-{}", job_id.0),
        },
    }
}

fn admission_boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
    let pool = WorkerPoolId(QUICK_DEVICE_WORKER_POOL_ID.to_owned());
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
            worker_pool_id: pool,
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

fn admission_error(
    error: &winwincode_storage::ExecutionAdmissionError,
) -> QuickDeviceDispatchError {
    if matches!(
        error.code(),
        ExecutionAdmissionErrorCode::QueueCapacityExhausted
            | ExecutionAdmissionErrorCode::ConcurrencyExhausted
            | ExecutionAdmissionErrorCode::TokenBudgetExhausted
            | ExecutionAdmissionErrorCode::CostBudgetExhausted
            | ExecutionAdmissionErrorCode::RepositoryWriteConflict
            | ExecutionAdmissionErrorCode::RevisionConflict
    ) {
        QuickDeviceDispatchError::new(
            QuickDeviceDispatchErrorKind::AdmissionUnavailable,
            "device dispatch admission is temporarily unavailable",
        )
    } else {
        QuickDeviceDispatchError::new(
            QuickDeviceDispatchErrorKind::DispatchConflict,
            "device dispatch admission rejected the reservation",
        )
    }
}

pub(crate) fn derived_id(prefix: &str, namespace: &[u8], identity: &str) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut hasher = Sha256::new();
    hasher.update(namespace);
    hasher.update([0]);
    hasher.update(identity.as_bytes());
    let digest = hasher.finalize();
    let mut derived = String::with_capacity(prefix.len() + 26);
    derived.push_str(prefix);
    for index in 0..26 {
        let byte = digest[index % digest.len()];
        let shift = (index % 5) * 2;
        derived.push(char::from(
            ALPHABET[usize::from((u16::from(byte) >> shift) & 0x1f)],
        ));
    }
    derived
}

const BINDING_ID_NAMESPACE: &[u8] = b"winwincode.quick-device-binding.v1";
const BIND_REQUEST_NAMESPACE: &[u8] = b"winwincode.quick-device-bind-request.v1";
const FACTS_REQUEST_NAMESPACE: &[u8] = b"winwincode.quick-device-facts.v1";
const RESERVATION_REQUEST_NAMESPACE: &[u8] = b"winwincode.quick-device-reservation.v1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ids_are_canonical_and_stable() {
        let first = derived_id("deb_", BINDING_ID_NAMESPACE, "wlg_A");
        assert_eq!(first.len(), 4 + 26);
        assert!(first.starts_with("deb_"));
        assert_eq!(first, derived_id("deb_", BINDING_ID_NAMESPACE, "wlg_A"));
        let request = derived_id("req_", FACTS_REQUEST_NAMESPACE, "job_A");
        assert!(request.starts_with("req_"));
        assert_ne!(first, request);
        let other = derived_id("deb_", BINDING_ID_NAMESPACE, "wlg_B");
        assert_ne!(first, other);
    }

    #[test]
    fn the_device_worker_pool_id_is_canonical() {
        let suffix = QUICK_DEVICE_WORKER_POOL_ID
            .strip_prefix("wpl_")
            .expect("device pool prefix");
        assert_eq!(suffix.len(), 26);
        assert!(suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        }));
    }
}
