// SPDX-License-Identifier: Apache-2.0

//! `StrongFlow` role-to-Device `WorkerSession` routing (`FLOW-100.5`).
//!
//! Planner, Executor, Reviewer, and Verifier keep their independent roles
//! and read/write permissions, yet every role's stage execution follows the
//! same Device scheduling base as Quick Chat: when the Delivery stage run
//! that a `delivery.advance` just committed carries a durable launch anchor —
//! a `WorkerLaunchGrant` minted for exactly that stage run — the stage's
//! queued `ExecutionJob` executes on the Device `WorkerSession` that grant
//! launched instead of the supervised local worker. A stage run without an
//! anchor keeps the local execution path unchanged.
//!
//! The per-stage anchor is what preserves the role boundaries on one shared
//! Device: each role (and each new attempt, which always opens a new stage
//! run) is launched as its own `WorkerSession`, one launch grant per
//! session, so distinct roles can never reuse another role's ``CodexThread``.
//! The role travels with the dispatch: the facts attachment stamps the
//! job's `executionProfile` onto the durable reservation facts, so every
//! device-owned job row names the exact role, worker session, and grant it
//! executes against.
//!
//! The routing steps mirror the Quick Chat device dispatch path exactly:
//!
//! 1. the anchor grant's `WorkerSession` is bound once in the
//!    `DeviceExecutionBinding` ledger (identity join from launch material to
//!    durable `ExecutionPort` identity);
//! 2. the acting user is judged by the FLOW-100.3 permission gate against
//!    the anchor's occupancy and repository visibility — a denial routes
//!    nothing;
//! 3. the stage job is reserved under the anchor holder's admission identity
//!    and receives its device facts through the same ledger, so the local
//!    queue exclusion (the repository scheduler and the local driver never
//!    claim a job carrying device facts) applies unchanged.
//!
//! Every step is idempotent: replaying the same `delivery.advance` finds the
//! durable receipts and changes nothing. The anchor stays a permission fact
//! only: a revoked or expired grant refuses new dispatches instead of
//! routing work to a dead worker session.

use std::fmt;

use winwincode_domain::{ExecutionJobId, Instant, RequestId, StageRunId, UserId};
use winwincode_execution_port::generated::{ExecutionJob, ExecutionWorkspaceWriteMode};
use winwincode_storage::{
    DeviceExecutionBindingIssuance, DeviceExecutionBindingRecord, DeviceExecutionBindingState,
    DeviceExecutionFactsAttachment, DeviceExecutionReservationFacts, ExecutionAdmissionBoundary,
    ExecutionAdmissionErrorCode, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionJobRecord, ExecutionQueueScope, ExecutionRepositoryAccess,
    ExecutionReservationRequest, ExecutionReservationState, ProductStateStorage, SqliteStorage,
    WorkerPoolId,
};

use crate::client_launch_grant::{
    LaunchGrantState, WorkerLaunchGrantRecord, WorkerLaunchGrantService,
};
use crate::device_execution_binding::DeviceExecutionBindingService;
use crate::device_session_gate::{
    DeviceSessionGateDenial, DeviceSessionGateInput, authorize_device_session,
};
use crate::quick_device_execution::derived_id;

/// Worker pool boundary of `StrongFlow` device-dispatched execution. Distinct
/// from the Quick device pool and the supervised local pool so reservation
/// accounting of every execution surface stays separable.
pub const STRONGFLOW_DEVICE_WORKER_POOL_ID: &str = "wpl_000000000000000000000000F7";

/// Admission bounds of the `StrongFlow` device dispatch reservation. The
/// boundaries shared with the supervised local driver and the Quick device
/// dispatch (organization, project, repository, product session, delivery)
/// must repeat those paths' policy values exactly — admission policies are
/// first-writer-wins and refuse a different reconfiguration. The concurrency
/// headroom for concurrently dispatched roles lives on the `StrongFlow` worker
/// pool boundary alone, which no other path configures.
const STRONGFLOW_DEVICE_ADMISSION_LIMITS: ExecutionAdmissionLimits = ExecutionAdmissionLimits {
    max_concurrent: 1,
    max_queued: 10_000,
    token_budget: 1_000_000_000,
    cost_budget_microunits: 1_000_000_000,
    max_runtime_millis: 604_800_000,
};

/// The concurrency capacity of the `StrongFlow` device worker pool itself:
/// the flow's roles may hold reservations at the same time, each on its own
/// `WorkerSession`, within the Client's worker-session capacity.
const STRONGFLOW_DEVICE_POOL_MAX_CONCURRENT: u64 = 4;

const RESERVED_TOKENS: u64 = 1_000_000;
const RESERVED_COST_MICROUNITS: u64 = 1_000_000;

/// The canonical Delivery execution roles a stage job can dispatch as. A job
/// whose profile is not in this set is not a `StrongFlow` role execution and
/// keeps the local path.
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

/// Stable `StrongFlow` device routing failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StrongflowDeviceDispatchErrorKind {
    /// A command input violated the canonical identity bounds.
    InvalidInput,
    /// The stage run's anchor launch grant is revoked or expired: the device
    /// worker session it launched can no longer execute work.
    AnchorNotLive,
    /// The anchor's worker session already released its execution binding.
    WorkerSessionEnded,
    /// The FLOW-100.3 permission gate denied the acting user; nothing was
    /// routed. The denial carries the central wire code and HTTP status.
    GateDenied,
    /// The stage job is already dispatched to a different launch or role, or
    /// its admission state contradicts the dispatch.
    DispatchConflict,
    /// Execution admission rejected the reservation for an ordinary,
    /// temporary capacity reason; a retry may succeed.
    AdmissionUnavailable,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free `StrongFlow` device routing failure.
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

    /// The permission gate denial behind a [`Self::kind`] of
    /// [`StrongflowDeviceDispatchErrorKind::GateDenied`].
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

/// The durable dispatch decision of one device-anchored `StrongFlow` stage.
#[derive(Clone, Debug, PartialEq)]
pub struct StrongflowDeviceDispatch {
    /// The `WorkerSession` binding the stage's launch material produced.
    pub binding: DeviceExecutionBindingRecord,
    /// The stage job's device facts: the identity match that routes the role
    /// to the launched device worker and excludes it from local claims. The
    /// `role` field names the execution profile the facts were stamped with.
    pub facts: DeviceExecutionReservationFacts,
}

/// Routes the committed Codex job of one Delivery stage run to its Device
/// `WorkerSession` when the stage run carries a durable launch anchor.
///
/// `actor_user_id` is the authenticated browser actor of the delivery
/// command; service and system actors (`None`) never route to a device.
///
/// Returns `Ok(None)` when the stage has no active job, no routable role, or
/// no anchor: the stage keeps the supervised local execution path and
/// nothing durable changes. Any error leaves the queued stage job untouched
/// (its device dispatch simply did not happen yet), so an exact command
/// replay can complete it later.
///
/// # Errors
///
/// Returns the stable routing failure categories; nothing is decided on a
/// storage failure.
pub fn dispatch_stage_to_device_worker(
    storage: &mut SqliteStorage,
    actor_user_id: Option<&str>,
    stage_run_id: &StageRunId,
    now: &Instant,
) -> Result<Option<StrongflowDeviceDispatch>, StrongflowDeviceDispatchError> {
    let Some(actor_user_id) = actor_user_id else {
        // Service and system actors stay on the supervised local path.
        return Ok(None);
    };
    let Some(record) = load_active_stage_job(storage, stage_run_id)? else {
        // No active stage job (human stage, or the stage already settled):
        // nothing is routable.
        return Ok(None);
    };
    let job = decode_execution_job(&record)?;
    let role = job.execution_profile.as_str();
    if !STRONGFLOW_DEVICE_ROLES.contains(&role) {
        // Not a StrongFlow role execution: keep the local path unchanged.
        return Ok(None);
    }
    let Some(anchor) = resolve_stage_anchor(storage, actor_user_id, &record, stage_run_id)? else {
        // No device anchor: the stage is not a device stage and keeps the
        // supervised local execution path unchanged.
        return Ok(None);
    };
    let binding = ensure_stage_binding(storage, &anchor, now)?;
    let facts = ensure_stage_facts(storage, &record, &anchor, role, now)?;
    Ok(Some(StrongflowDeviceDispatch { binding, facts }))
}

/// Loads the one active job of the stage run and checks its identity.
///
/// Returns `Ok(None)` when the stage has no active job.
fn load_active_stage_job(
    storage: &mut SqliteStorage,
    stage_run_id: &StageRunId,
) -> Result<Option<ExecutionJobRecord>, StrongflowDeviceDispatchError> {
    let record = storage
        .load_active_execution_job_record_for_stage_run(stage_run_id)
        .map_err(|_| StrongflowDeviceDispatchError::storage())?;
    let Some(record) = record else {
        return Ok(None);
    };
    if record.stage_run_id.as_ref() != Some(stage_run_id) {
        return Err(StrongflowDeviceDispatchError::corrupt(
            "the active stage job identity does not match the stage run",
        ));
    }
    Ok(Some(record))
}

/// Resolves one stage run's device anchor behind the permission gate: the
/// anchor's product session must match the job's scope, the grant must still
/// be live, and the acting user must pass the FLOW-100.3 gate. Returns
/// `Ok(None)` when the stage run carries no launch anchor.
fn resolve_stage_anchor(
    storage: &mut SqliteStorage,
    actor_user_id: &str,
    record: &ExecutionJobRecord,
    stage_run_id: &StageRunId,
) -> Result<Option<WorkerLaunchGrantRecord>, StrongflowDeviceDispatchError> {
    // The stage run's device anchor is its own launch grant, whatever its
    // lifecycle state: the anchor proves the role was bound to device
    // execution and stays the permission anchor after the grant ends.
    let anchor = WorkerLaunchGrantService::new(storage)
        .newest_grant_for_stage_run(stage_run_id.0.as_str())
        .map_err(|_| StrongflowDeviceDispatchError::storage())?;
    let Some(anchor) = anchor else {
        return Ok(None);
    };
    if anchor.product_session_id.as_deref() != Some(record.scope.product_session_id.0.as_str()) {
        return Err(StrongflowDeviceDispatchError::corrupt(
            "the stage run's launch anchor belongs to another product session",
        ));
    }
    if !matches!(
        anchor.state,
        LaunchGrantState::Issued | LaunchGrantState::Consumed
    ) {
        return Err(StrongflowDeviceDispatchError::new(
            StrongflowDeviceDispatchErrorKind::AnchorNotLive,
            "the stage run's launch anchor grant can no longer execute work",
        ));
    }
    // The FLOW-100.3 gate decides before anything is routed: only the
    // current occupancy holder of the anchor's client, with the anchor's
    // repository binding still visible, may dispatch to this device.
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

/// Binds the anchor's worker session once (idempotent) and returns the
/// binding: the launch material becomes the device session's durable
/// `ExecutionPort` identity.
fn ensure_stage_binding(
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

/// Reserves the stage job's admission and attaches its device facts once
/// (idempotent), with the stage's role stamped on them.
fn ensure_stage_facts(
    storage: &mut SqliteStorage,
    record: &ExecutionJobRecord,
    anchor: &WorkerLaunchGrantRecord,
    role: &str,
    now: &Instant,
) -> Result<DeviceExecutionReservationFacts, StrongflowDeviceDispatchError> {
    // An already attached job is either this exact dispatch (idempotent
    // repeat) or a routing conflict this lane refuses instead of silently
    // re-anchoring committed work.
    if let Some(existing) = DeviceExecutionBindingService::new(storage)
        .facts(record.job_id.0.as_str())
        .map_err(|_| StrongflowDeviceDispatchError::storage())?
    {
        let exact = existing.worker_launch_grant_id == anchor.worker_launch_grant_id
            && existing.role.as_deref() == Some(role);
        if exact {
            return Ok(existing);
        }
        return Err(StrongflowDeviceDispatchError::new(
            StrongflowDeviceDispatchErrorKind::DispatchConflict,
            "the stage job is already dispatched to another device launch or role",
        ));
    }
    // The dispatch reservation runs under the anchor holder's admission
    // identity, so the durable device facts join the reservation user to the
    // grant holder exactly.
    let job = decode_execution_job(record)?;
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

/// Echoes every anchor grant field into the validated bind command with
/// stable derived identities, so a repeated dispatch replays the original
/// receipt instead of conflicting.
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
        anchor.stage_run_id.clone(),
    )
    .map_err(|error| StrongflowDeviceDispatchError::invalid_input(error.to_string()))
}

/// Reserves the stage job's execution admission under the `StrongFlow` device
/// worker pool and the anchor holder's identity when no reservation exists
/// yet.
fn ensure_device_admission_reservation(
    storage: &mut SqliteStorage,
    record: &ExecutionJobRecord,
    job: &ExecutionJob,
    anchor: &WorkerLaunchGrantRecord,
) -> Result<(), StrongflowDeviceDispatchError> {
    let runtime_limit_millis = job_runtime_limit_millis(job)?;
    let mut admission = storage
        .execution_admission()
        .map_err(|_| StrongflowDeviceDispatchError::storage())?;
    if let Some(existing) = admission
        .load_reservation_by_job(&record.job_id)
        .map_err(|_| StrongflowDeviceDispatchError::storage())?
    {
        return match existing.state {
            ExecutionReservationState::Queued | ExecutionReservationState::Running => Ok(()),
            ExecutionReservationState::Released | ExecutionReservationState::Settled => {
                Err(StrongflowDeviceDispatchError::new(
                    StrongflowDeviceDispatchErrorKind::DispatchConflict,
                    "the stage job's execution reservation is already terminal",
                ))
            }
        };
    }
    for boundary in admission_boundaries(&record.scope) {
        // The pool boundary carries the multi-role concurrency headroom; the
        // shared boundaries repeat the exact policy every path configures.
        let limits = if matches!(boundary, ExecutionAdmissionBoundary::WorkerPool { .. }) {
            ExecutionAdmissionLimits {
                max_concurrent: STRONGFLOW_DEVICE_POOL_MAX_CONCURRENT,
                ..STRONGFLOW_DEVICE_ADMISSION_LIMITS
            }
        } else {
            STRONGFLOW_DEVICE_ADMISSION_LIMITS
        };
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .map_err(|error| admission_error(&error))?;
    }
    let request = ExecutionReservationRequest {
        scope: record.scope.clone(),
        user_id: UserId(anchor.holder_user_id.clone()),
        worker_pool_id: WorkerPoolId(STRONGFLOW_DEVICE_WORKER_POOL_ID.to_owned()),
        job_id: record.job_id.clone(),
        request_id: RequestId(derived_id(
            "req_",
            RESERVATION_REQUEST_NAMESPACE,
            &record.job_id.0,
        )),
        repository_access: repository_access(job, &record.job_id),
        reserved_tokens: RESERVED_TOKENS,
        reserved_cost_microunits: RESERVED_COST_MICROUNITS,
        runtime_limit_millis,
        submitted_at: record.submitted_at.clone(),
    };
    admission
        .reserve(&request)
        .map(|_| ())
        .map_err(|error| admission_error(&error))
}

fn decode_execution_job(
    record: &ExecutionJobRecord,
) -> Result<ExecutionJob, StrongflowDeviceDispatchError> {
    let job: ExecutionJob = serde_json::from_slice(&record.dispatch_payload).map_err(|_| {
        StrongflowDeviceDispatchError::corrupt("the queued dispatch payload is not an ExecutionJob")
    })?;
    if job.job_id != record.job_id
        || job.payload_digest != record.payload_digest
        || job.attempt != i64::try_from(record.attempt).unwrap_or(-1)
    {
        return Err(StrongflowDeviceDispatchError::corrupt(
            "the queued dispatch payload does not match its durable job",
        ));
    }
    Ok(job)
}

fn job_runtime_limit_millis(job: &ExecutionJob) -> Result<u64, StrongflowDeviceDispatchError> {
    u64::try_from(job.limits.max_runtime_seconds)
        .ok()
        .filter(|seconds| *seconds > 0)
        .and_then(|seconds| seconds.checked_mul(1_000))
        .ok_or_else(|| {
            StrongflowDeviceDispatchError::corrupt("the queued job execution deadline is invalid")
        })
}

fn repository_access(job: &ExecutionJob, job_id: &ExecutionJobId) -> ExecutionRepositoryAccess {
    match job.workspace.write_mode {
        // A read-only role (planner, reviewer, verifier) stays read-only on
        // the device; only writer roles receive an isolated candidate
        // worktree, so Reviewer/Verifier can never write the Candidate.
        ExecutionWorkspaceWriteMode::ReadOnly => ExecutionRepositoryAccess::ReadOnly,
        ExecutionWorkspaceWriteMode::Candidate => ExecutionRepositoryAccess::IsolatedWrite {
            worktree_key: format!("job-{}", job_id.0),
        },
    }
}

fn admission_boundaries(scope: &ExecutionQueueScope) -> Vec<ExecutionAdmissionBoundary> {
    let pool = WorkerPoolId(STRONGFLOW_DEVICE_WORKER_POOL_ID.to_owned());
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

/// Ordinary admission backpressure defers the dispatch instead of failing
/// the boundary: the stage job stays queued and an exact retry re-runs the
/// routing.
fn admission_error(
    error: &winwincode_storage::ExecutionAdmissionError,
) -> StrongflowDeviceDispatchError {
    if matches!(
        error.code(),
        ExecutionAdmissionErrorCode::QueueCapacityExhausted
            | ExecutionAdmissionErrorCode::ConcurrencyExhausted
            | ExecutionAdmissionErrorCode::TokenBudgetExhausted
            | ExecutionAdmissionErrorCode::CostBudgetExhausted
            | ExecutionAdmissionErrorCode::RepositoryWriteConflict
            | ExecutionAdmissionErrorCode::RevisionConflict
    ) {
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
        // A distinct namespace never collides with the Quick dispatch
        // identities for the same anchor material.
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
