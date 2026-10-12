// SPDX-License-Identifier: Apache-2.0

//! Quick Chat native Codex continuous execution path (FLOW-100.4).
//!
//! After a `chat.submit` command passes the FLOW-100.3 permission gate and
//! commits its turn, the execution position of that turn follows the
//! session's durable launch anchor: a session whose newest
//! `WorkerLaunchGrant` anchors it to a Client executes on the Device
//! `WorkerSession` that the grant launched, and a session without an anchor
//! keeps the supervised local execution path unchanged.
//!
//! The dispatch is a durable identity join, not a transport action:
//!
//! 1. the anchor grant's `WorkerSession` is bound once in the
//!    `DeviceExecutionBinding` ledger — the launch-response material
//!    (`workerSessionId`/`workerId`/`workerInstanceId` beside the occupancy
//!    and repository facts) becomes the queryable `ExecutionPort` identity of
//!    the device session;
//! 2. the turn's queued `ExecutionJob` is reserved under the anchor holder's
//!    admission identity and receives its device facts through the same
//!    ledger (`attach_facts`): one row naming the exact device worker
//!    identities, occupancy lease, and repository binding the job must
//!    execute against. Queue selection and the local runtime scheduler treat
//!    a job with device facts as device-owned and never claim it locally, so
//!    the device worker — the process the Client launched against its
//!    session credential — is the only possible executor of the turn.
//!
//! Every step is idempotent: repeated turns, command replays, and a crashed
//! dispatch between two steps all find their durable receipts and change
//! nothing. The anchor stays a permission fact only: a revoked or expired
//! grant refuses new dispatches instead of routing work to a dead worker
//! session (replacement and drain semantics belong to FLOW-100.6).

use std::fmt;

use sha2::{Digest, Sha256};
use winwincode_domain::{ExecutionJobId, Instant, ProductSessionId, RequestId, UserId};
use winwincode_storage::{
    DeviceExecutionBindingIssuance, DeviceExecutionBindingRecord, DeviceExecutionBindingState,
    DeviceExecutionFactsAttachment, DeviceExecutionReservationFacts, ExecutionJobRecord,
    ExecutionJobState, ExecutionReservationState, ProductStateStorage, SqliteStorage, WorkerPoolId,
};

use crate::execution_admission::{
    execution_admission_boundaries, execution_admission_deferred, execution_admission_job,
    execution_reservation_request,
};

use crate::client_launch_grant::{
    LaunchGrantState, WorkerLaunchGrantRecord, WorkerLaunchGrantService,
};
use crate::device_execution_binding::DeviceExecutionBindingService;
use crate::product_session_service::chat_turn_execution_job_id;

/// Worker pool boundary of device-dispatched execution. Distinct from the
/// supervised local pool so a device reservation never consumes local
/// admission capacity and the local driver can never mistake device work
/// for its own.
pub const QUICK_DEVICE_WORKER_POOL_ID: &str = "wpl_000000000000000000000000D3";

/// Stable Quick device dispatch failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuickDeviceDispatchErrorKind {
    /// A command input violated the canonical identity bounds.
    InvalidInput,
    /// The session's anchor launch grant is revoked or expired: the device
    /// worker session it launched can no longer execute work.
    AnchorNotLive,
    /// The anchor's worker session already released its execution binding.
    WorkerSessionEnded,
    /// The turn is already dispatched to a different launch, or its
    /// admission state contradicts the dispatch.
    DispatchConflict,
    /// Execution admission rejected the reservation for an ordinary,
    /// temporary capacity reason; a retry may succeed.
    AdmissionUnavailable,
    /// A stored row violated the frozen schema invariants.
    CorruptState,
    /// The underlying storage operation failed.
    Storage,
}

/// Secret-free Quick device dispatch failure.
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

/// The durable dispatch decision of one device-anchored Chat turn.
#[derive(Clone, Debug, PartialEq)]
pub struct QuickDeviceDispatch {
    /// The `WorkerSession` binding the launch material produced.
    pub binding: DeviceExecutionBindingRecord,
    /// The job's device facts: the identity match that routes the turn to
    /// the launched device worker and excludes it from local claims.
    pub facts: DeviceExecutionReservationFacts,
}

/// Routes one committed Chat turn to its session's Device `WorkerSession`
/// when the session carries a durable launch anchor.
///
/// Returns `Ok(None)` when the session has no anchor: the turn keeps the
/// supervised local execution path and nothing durable changes. Any error
/// leaves the queued turn untouched (its device dispatch simply did not
/// happen yet), so an exact command replay can complete it later.
///
/// # Errors
///
/// Returns the stable dispatch failure categories; nothing is decided on a
/// storage failure.
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
        // No device anchor: the session is not a device session and keeps
        // the supervised local execution path unchanged.
        return Ok(None);
    };
    dispatch_chat_job_to_device_worker(storage, &execution_job_id, &anchor, now).map(Some)
}

/// Resumes the original Chat job through its sealed Device launch lineage.
/// No new turn, job or admission reservation is created for an existing route.
/// # Errors
/// Rejects cross-session, role, cancellation and unsealed replacement authority.
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
        || anchor.work_run_id.is_some()
        || anchor.product_session_id.as_deref() != Some(&record.scope.product_session_id.0)
    {
        return Err(QuickDeviceDispatchError::invalid_input(
            "the Chat execution scope differs from its launch",
        ));
    }
    if matches!(
        record.state,
        ExecutionJobState::Completed | ExecutionJobState::Failed
    ) {
        return replay_terminal_dispatch(storage, &record, anchor);
    }
    validate_active_dispatch(&record, anchor)?;

    // The launch material becomes the device session's durable ExecutionPort
    // identity: bound exactly once per launch grant, replay-safe.
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
    // An already attached job is either this exact dispatch (idempotent
    // repeat) or a routing conflict this lane refuses instead of silently
    // re-anchoring committed work.
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

    // The dispatch reservation runs under the anchor holder's admission
    // identity, so the durable device facts join the reservation user to the
    // grant holder exactly.
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

fn validate_active_dispatch(
    record: &ExecutionJobRecord,
    anchor: &WorkerLaunchGrantRecord,
) -> Result<(), QuickDeviceDispatchError> {
    if record.cancellation.is_some() {
        return Err(QuickDeviceDispatchError::invalid_input(
            "the Chat execution already has a cancellation intent",
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
    Ok(())
}

// A terminal job is only a receipt replay. Read its original dispatch join
// before live-slot checks, and never bind, reserve, attach or stage recovery.
fn replay_terminal_dispatch(
    storage: &mut SqliteStorage,
    record: &ExecutionJobRecord,
    anchor: &WorkerLaunchGrantRecord,
) -> Result<QuickDeviceDispatch, QuickDeviceDispatchError> {
    let mut bindings = DeviceExecutionBindingService::new(storage);
    let facts = bindings
        .facts(&record.job_id.0)
        .map_err(|_| QuickDeviceDispatchError::storage())?
        .ok_or_else(|| {
            QuickDeviceDispatchError::corrupt("terminal Chat job lost its dispatch facts")
        })?;
    let binding = bindings
        .snapshot(&facts.worker_session_id)
        .map_err(|_| QuickDeviceDispatchError::storage())?
        .ok_or_else(|| {
            QuickDeviceDispatchError::corrupt("terminal Chat job lost its device binding")
        })?;
    if facts.job_id != record.job_id.0
        || facts.work_run_id.is_some()
        || facts.role.is_some()
        || facts.product_session_id.as_deref() != Some(&record.scope.product_session_id.0)
        || facts.worker_launch_grant_id != anchor.worker_launch_grant_id
        || facts.worker_session_id != anchor.worker_session_id
        || facts.worker_id != anchor.worker_id
        || facts.worker_instance_id != anchor.worker_instance_id
        || facts.client_node_id != anchor.client_node_id
        || facts.client_instance_id != anchor.client_instance_id
        || facts.holder_user_id != anchor.holder_user_id
        || facts.repository_binding_id != anchor.repository_binding_id
        || facts.occupancy_lease_id != anchor.occupancy_lease_id
        || facts.occupancy_fencing_token != anchor.occupancy_fencing_token
        || binding.worker_session_id != facts.worker_session_id
        || binding.worker_launch_grant_id != facts.worker_launch_grant_id
        || binding.product_session_id != facts.product_session_id
        || binding.work_run_id.is_some()
        || binding.client_node_id != facts.client_node_id
        || binding.client_instance_id != facts.client_instance_id
        || binding.holder_user_id != facts.holder_user_id
        || binding.repository_binding_id != facts.repository_binding_id
        || binding.occupancy_lease_id != facts.occupancy_lease_id
        || binding.occupancy_fencing_token != facts.occupancy_fencing_token
    {
        return Err(QuickDeviceDispatchError::new(
            QuickDeviceDispatchErrorKind::DispatchConflict,
            "terminal Chat dispatch differs from its original launch identity",
        ));
    }
    Ok(QuickDeviceDispatch { binding, facts })
}

/// Echoes every anchor grant field into the validated bind command with
/// stable derived identities, so a repeated dispatch replays the original
/// receipt instead of conflicting.
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

/// Reserves the turn's execution admission under the device worker pool and
/// the anchor holder's identity when no reservation exists yet.
fn ensure_device_admission_reservation(
    storage: &mut SqliteStorage,
    execution_job_id: &ExecutionJobId,
    anchor: &WorkerLaunchGrantRecord,
) -> Result<(), QuickDeviceDispatchError> {
    let record = load_queued_job(storage, execution_job_id)?;
    let (job, runtime_limit_millis) = execution_admission_job(&record)
        .map_err(|error| QuickDeviceDispatchError::corrupt(error.to_string()))?;
    let mut admission = storage
        .execution_admission()
        .map_err(|_| QuickDeviceDispatchError::storage())?;
    for boundary in execution_admission_boundaries(
        &record.scope,
        &WorkerPoolId(QUICK_DEVICE_WORKER_POOL_ID.to_owned()),
    ) {
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
    let request = execution_reservation_request(
        &record,
        &job,
        UserId(anchor.holder_user_id.clone()),
        WorkerPoolId(QUICK_DEVICE_WORKER_POOL_ID.to_owned()),
        RequestId(derived_id(
            "req_",
            RESERVATION_REQUEST_NAMESPACE,
            &record.job_id.0,
        )),
    )
    .map_err(|error| QuickDeviceDispatchError::corrupt(error.to_string()))?;
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

/// Ordinary admission backpressure defers the dispatch instead of failing
/// the boundary: the turn stays queued and an exact retry re-runs the glue.
fn admission_error(
    error: &winwincode_storage::ExecutionAdmissionError,
) -> QuickDeviceDispatchError {
    if execution_admission_deferred(error.code()) {
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

/// One canonical `prefix` + 26 character Crockford identity derived from
/// stable dispatch material, so every replay of the same dispatch reuses the
/// exact same command identities. Shared with the `StrongFlow` device routing,
/// which derives its identities from distinct namespaces.
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
