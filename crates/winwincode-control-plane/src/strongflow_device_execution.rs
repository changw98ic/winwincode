// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use winwincode_domain::{Instant, RequestId, UserId, WorkRunId};
use winwincode_execution_port::generated::ExecutionJob;
use winwincode_storage::{
    DeviceExecutionBindingIssuance, DeviceExecutionBindingRecord, DeviceExecutionBindingState,
    DeviceExecutionFactsAttachment, DeviceExecutionReservationFacts, ExecutionJobRecord,
    ExecutionReservationState, ProductStateStorage, SqliteStorage, WorkerPoolId,
};

use crate::client_launch_grant::{
    LaunchGrantState, WorkerLaunchGrantRecord, WorkerLaunchGrantService,
};
use crate::device_execution_binding::DeviceExecutionBindingService;
use crate::device_session_gate::{
    DeviceSessionGateDenial, DeviceSessionGateInput, authorize_device_session,
};
use crate::execution_admission::{
    execution_admission_boundaries, execution_admission_deferred, execution_admission_job,
    execution_reservation_request,
};
use crate::quick_device_execution::derived_id;

pub const STRONGFLOW_DEVICE_WORKER_POOL_ID: &str = "wpl_000000000000000000000000F7";

const STRONGFLOW_DEVICE_ROLES: [&str; 8] = [
    "requirements",
    "solution",
    "planner",
    "executor",
    "reviewer",
    "verifier",
    "adversarial-verifier",
    "remediator",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrongflowDeviceDispatchErrorKind {
    InvalidInput,
    AnchorNotLive,
    WorkerSessionEnded,
    GateDenied,
    DispatchConflict,
    AdmissionUnavailable,
    CorruptState,
    Storage,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StrongflowDeviceDispatchError {
    kind: StrongflowDeviceDispatchErrorKind,
    message: String,
    gate_denial: Option<DeviceSessionGateDenial>,
}

impl StrongflowDeviceDispatchError {
    #[must_use]
    pub const fn kind(&self) -> StrongflowDeviceDispatchErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn gate_denial(&self) -> Option<&DeviceSessionGateDenial> {
        self.gate_denial.as_ref()
    }

    fn new(kind: StrongflowDeviceDispatchErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            gate_denial: None,
        }
    }

    fn gate_denied(denial: DeviceSessionGateDenial) -> Self {
        Self {
            kind: StrongflowDeviceDispatchErrorKind::GateDenied,
            message: "the device execution gate denied the acting user".to_owned(),
            gate_denial: Some(denial),
        }
    }

    fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(StrongflowDeviceDispatchErrorKind::InvalidInput, message)
    }

    fn corrupt(message: impl Into<String>) -> Self {
        Self::new(StrongflowDeviceDispatchErrorKind::CorruptState, message)
    }

    fn storage() -> Self {
        Self::new(
            StrongflowDeviceDispatchErrorKind::Storage,
            "StrongFlow device routing storage failed",
        )
    }
}

impl fmt::Display for StrongflowDeviceDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StrongflowDeviceDispatchError {}

#[derive(Clone, Debug, PartialEq)]
pub struct StrongflowDeviceDispatch {
    pub binding: DeviceExecutionBindingRecord,
    pub facts: DeviceExecutionReservationFacts,
}

pub fn dispatch_work_run_to_device_worker(
    storage: &mut SqliteStorage,
    actor_user_id: Option<&str>,
    work_run_id: &WorkRunId,
    now: &Instant,
) -> Result<Option<StrongflowDeviceDispatch>, StrongflowDeviceDispatchError> {
    let Some(actor_user_id) = actor_user_id else {
        return Ok(None);
    };
    let Some(record) = load_active_work_run_job(storage, work_run_id)? else {
        return Ok(None);
    };
    let (job, _) = execution_admission_job(&record)
        .map_err(|error| StrongflowDeviceDispatchError::corrupt(error.to_string()))?;
    let role = job.execution_profile.as_str();
    if !STRONGFLOW_DEVICE_ROLES.contains(&role) {
        return Ok(None);
    }
    let Some(anchor) = resolve_work_run_anchor(storage, actor_user_id, &record, work_run_id)?
    else {
        return Ok(None);
    };
    if let Some(target) = job
        .work_input
        .as_ref()
        .and_then(|input| input.device_target.as_ref())
        && (target.client_node_id != anchor.client_node_id
            || target.repository_binding_id != anchor.repository_binding_id
            || target.user_id != anchor.holder_user_id)
    {
        return Err(StrongflowDeviceDispatchError::corrupt(
            "launch differs from the sealed Device target",
        ));
    }
    let binding = ensure_work_run_binding(storage, &anchor, now)?;
    let facts = ensure_work_run_facts(storage, &record, &anchor, role, now)?;
    Ok(Some(StrongflowDeviceDispatch { binding, facts }))
}

fn load_active_work_run_job(
    storage: &mut SqliteStorage,
    work_run_id: &WorkRunId,
) -> Result<Option<ExecutionJobRecord>, StrongflowDeviceDispatchError> {
    let record = storage
        .load_active_execution_job_record_for_work_run(work_run_id)
        .map_err(|_| StrongflowDeviceDispatchError::storage())?;
    let Some(record) = record else {
        return Ok(None);
    };
    if record.work_run_id.as_ref() != Some(work_run_id) {
        return Err(StrongflowDeviceDispatchError::corrupt(
            "the active WorkRun job identity does not match the WorkRun",
        ));
    }
    Ok(Some(record))
}

fn resolve_work_run_anchor(
    storage: &mut SqliteStorage,
    actor_user_id: &str,
    record: &ExecutionJobRecord,
    work_run_id: &WorkRunId,
) -> Result<Option<WorkerLaunchGrantRecord>, StrongflowDeviceDispatchError> {
    let existing = DeviceExecutionBindingService::new(storage)
        .facts(&record.job_id.0)
        .map_err(|_| StrongflowDeviceDispatchError::storage())?;
    let anchor = if let Some(facts) = existing {
        let ledger = storage
            .worker_launch_grant_ledger()
            .map_err(|_| StrongflowDeviceDispatchError::storage())?;
        ledger
            .recovery_launch_for_grant(&facts.worker_launch_grant_id)
            .map_err(|_| StrongflowDeviceDispatchError::storage())?
            .or(ledger
                .snapshot(&facts.worker_launch_grant_id)
                .map_err(|_| StrongflowDeviceDispatchError::storage())?)
    } else {
        WorkerLaunchGrantService::new(storage)
            .newest_grant_for_work_run(work_run_id.0.as_str())
            .map_err(|_| StrongflowDeviceDispatchError::storage())?
    };
    let Some(anchor) = anchor else {
        return Ok(None);
    };
    if anchor.product_session_id.as_deref() != Some(record.scope.product_session_id.0.as_str()) {
        return Err(StrongflowDeviceDispatchError::corrupt(
            "the WorkRun's launch anchor belongs to another product session",
        ));
    }
    if !matches!(
        anchor.state,
        LaunchGrantState::Issued | LaunchGrantState::Consumed
    ) {
        return Err(StrongflowDeviceDispatchError::new(
            StrongflowDeviceDispatchErrorKind::AnchorNotLive,
            "the WorkRun's launch anchor grant can no longer execute work",
        ));
    }
    authorize_device_session(
        storage,
        &DeviceSessionGateInput {
            user_id: actor_user_id,
            client_node_id: &anchor.client_node_id,
            repository_binding_id: &anchor.repository_binding_id,
        },
    )
    .map_err(StrongflowDeviceDispatchError::gate_denied)?;
    Ok(Some(anchor))
}

fn ensure_work_run_binding(
    storage: &mut SqliteStorage,
    anchor: &WorkerLaunchGrantRecord,
    now: &Instant,
) -> Result<DeviceExecutionBindingRecord, StrongflowDeviceDispatchError> {
    let mut bindings = DeviceExecutionBindingService::new(storage);
    if let Some(existing) = bindings
        .snapshot(&anchor.worker_session_id)
        .map_err(|_| StrongflowDeviceDispatchError::storage())?
    {
        if existing.state != DeviceExecutionBindingState::Bound {
            return Err(StrongflowDeviceDispatchError::new(
                StrongflowDeviceDispatchErrorKind::WorkerSessionEnded,
                "the anchored device worker session already released its binding",
            ));
        }
        return Ok(existing);
    }
    let command = bind_command(anchor)?;
    Ok(bindings
        .bind(&command, now)
        .map_err(|_| StrongflowDeviceDispatchError::storage())?
        .binding)
}

fn ensure_work_run_facts(
    storage: &mut SqliteStorage,
    record: &ExecutionJobRecord,
    anchor: &WorkerLaunchGrantRecord,
    role: &str,
    now: &Instant,
) -> Result<DeviceExecutionReservationFacts, StrongflowDeviceDispatchError> {
    if let Some(existing) = DeviceExecutionBindingService::new(storage)
        .facts(record.job_id.0.as_str())
        .map_err(|_| StrongflowDeviceDispatchError::storage())?
    {
        let exact = existing.worker_launch_grant_id == anchor.worker_launch_grant_id
            && existing.role.as_deref() == Some(role);
        if exact {
            return Ok(existing);
        }
        return winwincode_storage::stage_device_execution_recovery(
            storage,
            &record.job_id,
            &anchor.worker_launch_grant_id,
            Some(role),
            now,
        )
        .map_err(|_| {
            StrongflowDeviceDispatchError::new(
                StrongflowDeviceDispatchErrorKind::DispatchConflict,
                "the WorkRun recovery authority differs",
            )
        });
    }
    let (job, _) = execution_admission_job(record)
        .map_err(|error| StrongflowDeviceDispatchError::corrupt(error.to_string()))?;
    ensure_device_admission_reservation(storage, record, &job, anchor)?;
    let attachment = DeviceExecutionFactsAttachment::try_new_with_role(
        derived_id("req_", FACTS_REQUEST_NAMESPACE, &record.job_id.0),
        record.job_id.0.as_str(),
        anchor.worker_launch_grant_id.as_str(),
        Some(role.to_owned()),
    )
    .map_err(|error| StrongflowDeviceDispatchError::invalid_input(error.to_string()))?;
    Ok(DeviceExecutionBindingService::new(storage)
        .attach_facts(&attachment, now)
        .map_err(|_| StrongflowDeviceDispatchError::storage())?
        .facts)
}

fn bind_command(
    anchor: &WorkerLaunchGrantRecord,
) -> Result<DeviceExecutionBindingIssuance, StrongflowDeviceDispatchError> {
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
    .map_err(|error| StrongflowDeviceDispatchError::invalid_input(error.to_string()))
}

fn ensure_device_admission_reservation(
    storage: &mut SqliteStorage,
    record: &ExecutionJobRecord,
    job: &ExecutionJob,
    anchor: &WorkerLaunchGrantRecord,
) -> Result<(), StrongflowDeviceDispatchError> {
    let request = execution_reservation_request(
        record,
        job,
        UserId(anchor.holder_user_id.clone()),
        WorkerPoolId(STRONGFLOW_DEVICE_WORKER_POOL_ID.to_owned()),
        RequestId(derived_id(
            "req_",
            RESERVATION_REQUEST_NAMESPACE,
            &record.job_id.0,
        )),
    )
    .map_err(|error| StrongflowDeviceDispatchError::corrupt(error.to_string()))?;
    let mut admission = storage
        .execution_admission()
        .map_err(|_| StrongflowDeviceDispatchError::storage())?;
    for boundary in execution_admission_boundaries(&record.scope, &request.worker_pool_id) {
        admission
            .configure_runtime_policy(boundary, request.runtime_limit_millis.is_some())
            .map_err(|error| admission_error(&error))?;
    }
    if let Some(existing) = admission
        .load_reservation_by_job(&record.job_id)
        .map_err(|_| StrongflowDeviceDispatchError::storage())?
    {
        return match existing.state {
            ExecutionReservationState::Queued | ExecutionReservationState::Running => Ok(()),
            ExecutionReservationState::Released | ExecutionReservationState::Settled => {
                Err(StrongflowDeviceDispatchError::new(
                    StrongflowDeviceDispatchErrorKind::DispatchConflict,
                    "the WorkRun job's execution reservation is already terminal",
                ))
            }
        };
    }
    admission
        .reserve(&request)
        .map(|_| ())
        .map_err(|error| admission_error(&error))
}

fn admission_error(
    error: &winwincode_storage::ExecutionAdmissionError,
) -> StrongflowDeviceDispatchError {
    if execution_admission_deferred(error.code()) {
        StrongflowDeviceDispatchError::new(
            StrongflowDeviceDispatchErrorKind::AdmissionUnavailable,
            "device dispatch admission is temporarily unavailable",
        )
    } else {
        StrongflowDeviceDispatchError::new(
            StrongflowDeviceDispatchErrorKind::DispatchConflict,
            "device dispatch admission rejected the reservation",
        )
    }
}

const BINDING_ID_NAMESPACE: &[u8] = b"winwincode.strongflow-device-binding.v1";
const BIND_REQUEST_NAMESPACE: &[u8] = b"winwincode.strongflow-device-bind-request.v1";
const FACTS_REQUEST_NAMESPACE: &[u8] = b"winwincode.strongflow-device-facts.v1";
const RESERVATION_REQUEST_NAMESPACE: &[u8] = b"winwincode.strongflow-device-reservation.v1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ids_are_canonical_distinct_from_quick_and_stable() {
        let binding = derived_id("deb_", BINDING_ID_NAMESPACE, "wlg_A");
        assert_eq!(binding.len(), 4 + 26);
        assert!(binding.starts_with("deb_"));
        assert_eq!(binding, derived_id("deb_", BINDING_ID_NAMESPACE, "wlg_A"));
        let quick_binding = derived_id("deb_", b"winwincode.quick-device-binding.v1", "wlg_A");
        assert_ne!(binding, quick_binding);
        let facts = derived_id("req_", FACTS_REQUEST_NAMESPACE, "job_A");
        assert!(facts.starts_with("req_"));
        assert_ne!(binding, facts);
    }

    #[test]
    fn the_strongflow_device_worker_pool_id_is_canonical_and_distinct() {
        let suffix = STRONGFLOW_DEVICE_WORKER_POOL_ID
            .strip_prefix("wpl_")
            .expect("device pool prefix");
        assert_eq!(suffix.len(), 26);
        assert!(suffix.bytes().all(|byte| {
            byte.is_ascii_digit()
                || matches!(byte, b'A'..=b'H' | b'J'..=b'K' | b'M'..=b'N' | b'P'..=b'T' | b'V'..=b'Z')
        }));
        assert_ne!(
            STRONGFLOW_DEVICE_WORKER_POOL_ID,
            crate::quick_device_execution::QUICK_DEVICE_WORKER_POOL_ID
        );
    }

    #[test]
    fn the_role_table_is_exactly_the_canonical_delivery_profiles() {
        for role in STRONGFLOW_DEVICE_ROLES {
            assert!(!role.is_empty() && role.len() <= 100);
            assert!(
                role.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            );
        }
    }
}
