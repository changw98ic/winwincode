// SPDX-License-Identifier: Apache-2.0

//! Exact execution-session binding transitions.

use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, ProductSessionId, StageRunId,
    WorkerSessionId,
};

use crate::domain::{Delivery, StageRunStatus};

use super::{CoordinationError, CoordinationErrorCode, require_mutation_time};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingIdentity {
    pub delivery_id: DeliveryId,
    pub delivery_task_id: Option<DeliveryTaskId>,
    pub stage_run_id: StageRunId,
    pub product_session_id: ProductSessionId,
    pub execution_job_id: ExecutionJobId,
}

/// Records the `WorkerSession` returned by an accepted immutable dispatch.
///
/// # Errors
///
/// Fails closed on stale revision, a non-active run, any identity mismatch, or
/// a `WorkerSession` already owned by another binding.
pub fn accept_worker_session(
    delivery: &Delivery,
    expected_revision: u64,
    identity: &SessionBindingIdentity,
    worker_session_id: WorkerSessionId,
    now_millis: u64,
) -> Result<Delivery, CoordinationError> {
    require_revision(delivery, expected_revision)?;
    require_mutation_time(delivery, now_millis)?;
    let index = exact_binding_index(delivery, identity)?;
    require_active_run(delivery, identity)?;
    if delivery
        .snapshot()
        .session_bindings
        .iter()
        .enumerate()
        .any(|(other_index, binding)| {
            other_index != index && binding.worker_session_id.as_ref() == Some(&worker_session_id)
        })
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "WorkerSession is already assigned to another StageRun",
        ));
    }
    let current = &delivery.snapshot().session_bindings[index];
    if current.worker_session_id.as_ref() == Some(&worker_session_id) {
        return Ok(delivery.clone());
    }
    if current.worker_session_id.is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "SessionBinding already has another WorkerSession",
        ));
    }
    let mut snapshot = delivery.clone().into_snapshot();
    snapshot.session_bindings[index].worker_session_id = Some(worker_session_id);
    snapshot.revision += 1;
    snapshot.updated_at_millis = now_millis;
    Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::BindingConflict, error.to_string())
    })
}

/// Records the `CodexThread` reported by the already accepted `WorkerSession`.
///
/// # Errors
///
/// Fails closed on stale revision, any exact identity mismatch, a changed
/// `WorkerSession`, or a `CodexThread` already owned by another binding.
pub fn report_codex_thread(
    delivery: &Delivery,
    expected_revision: u64,
    identity: &SessionBindingIdentity,
    worker_session_id: &WorkerSessionId,
    codex_thread_id: CodexThreadId,
    now_millis: u64,
) -> Result<Delivery, CoordinationError> {
    require_revision(delivery, expected_revision)?;
    require_mutation_time(delivery, now_millis)?;
    let index = exact_binding_index(delivery, identity)?;
    require_active_run(delivery, identity)?;
    let current = &delivery.snapshot().session_bindings[index];
    if current.worker_session_id.as_ref() != Some(worker_session_id) {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "CodexThread report does not match the accepted WorkerSession",
        ));
    }
    if delivery
        .snapshot()
        .session_bindings
        .iter()
        .enumerate()
        .any(|(other_index, binding)| {
            other_index != index && binding.codex_thread_id.as_ref() == Some(&codex_thread_id)
        })
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "CodexThread is already assigned to another StageRun",
        ));
    }
    if current.codex_thread_id.as_ref() == Some(&codex_thread_id) {
        return Ok(delivery.clone());
    }
    if current.codex_thread_id.is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "SessionBinding already has another CodexThread",
        ));
    }
    let mut snapshot = delivery.clone().into_snapshot();
    snapshot.session_bindings[index].codex_thread_id = Some(codex_thread_id);
    snapshot.revision += 1;
    snapshot.updated_at_millis = now_millis;
    Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::BindingConflict, error.to_string())
    })
}

fn require_revision(delivery: &Delivery, expected_revision: u64) -> Result<(), CoordinationError> {
    if delivery.revision() == expected_revision {
        Ok(())
    } else {
        Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before SessionBinding update",
        ))
    }
}

fn exact_binding_index(
    delivery: &Delivery,
    identity: &SessionBindingIdentity,
) -> Result<usize, CoordinationError> {
    let mut matches = delivery
        .snapshot()
        .session_bindings
        .iter()
        .enumerate()
        .filter(|(_, binding)| {
            binding.delivery_id == identity.delivery_id
                && binding.delivery_task_id == identity.delivery_task_id
                && binding.stage_run_id == identity.stage_run_id
                && binding.product_session_id == identity.product_session_id
                && binding.execution_job_id == identity.execution_job_id
        });
    let (index, _) = matches.next().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "no SessionBinding matches the exact Delivery stage and job identity",
        )
    })?;
    if matches.next().is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "more than one SessionBinding matches the stage and job identity",
        ));
    }
    Ok(index)
}

fn require_active_run(
    delivery: &Delivery,
    identity: &SessionBindingIdentity,
) -> Result<(), CoordinationError> {
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == identity.stage_run_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::BindingConflict,
                "SessionBinding StageRun does not exist",
            )
        })?;
    if run.delivery_id != identity.delivery_id
        || run.delivery_task_id != identity.delivery_task_id
        || !matches!(
            run.status,
            StageRunStatus::Running | StageRunStatus::Waiting
        )
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "SessionBinding does not match an active exact StageRun",
        ));
    }
    Ok(())
}
