// SPDX-License-Identifier: Apache-2.0

//! Approved DeliveryTask graph transitions.

use crate::domain::{
    AttentionItemStatus, AttentionItemType, Delivery, DeliveryStage, DeliveryStatus, DeliveryTask,
    DeliveryTaskStatus, StageRunStatus,
};

use super::{CoordinationError, CoordinationErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFact {
    StartExecuting,
    StartVerifying,
    VerificationPassed,
    VerificationFailed,
    StartReworking,
    ExecutingCancelled,
    VerifyingCancelled,
    ReworkingCancelled,
}

/// Applies one Delivery-owned fact to a task status.
///
/// # Errors
///
/// Rejects free-form or out-of-order status changes.
pub fn transition_task_status(
    status: DeliveryTaskStatus,
    fact: TaskFact,
) -> Result<DeliveryTaskStatus, CoordinationError> {
    let next = match (status, fact) {
        (DeliveryTaskStatus::Pending, TaskFact::StartExecuting) => DeliveryTaskStatus::Active,
        (DeliveryTaskStatus::Active, TaskFact::StartVerifying) => DeliveryTaskStatus::Verifying,
        (DeliveryTaskStatus::Verifying, TaskFact::StartVerifying) => {
            DeliveryTaskStatus::Verifying
        }
        (DeliveryTaskStatus::Verifying, TaskFact::VerificationPassed) => {
            DeliveryTaskStatus::Completed
        }
        (DeliveryTaskStatus::Verifying, TaskFact::VerificationFailed) => {
            DeliveryTaskStatus::Failed
        }
        (DeliveryTaskStatus::Failed, TaskFact::StartReworking) => DeliveryTaskStatus::Active,
        (DeliveryTaskStatus::Active, TaskFact::ExecutingCancelled) => DeliveryTaskStatus::Pending,
        (DeliveryTaskStatus::Verifying, TaskFact::VerifyingCancelled) => {
            DeliveryTaskStatus::Verifying
        }
        (DeliveryTaskStatus::Active, TaskFact::ReworkingCancelled) => DeliveryTaskStatus::Failed,
        _ => {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "DeliveryTask status does not accept this Delivery fact",
            ));
        }
    };
    Ok(next)
}

/// Approves one immutable DeliveryTask graph after the current plan review.
///
/// `delivery.create.tasks` remains empty. This command is the only normal path
/// that writes the task graph, once per current Spec revision.
///
/// # Errors
///
/// Fails closed on stale revision, missing current plan approval, an existing
/// graph, empty proposals, non-pending task state, or an invalid task DAG.
pub fn approve_task_breakdown(
    delivery: &Delivery,
    expected_revision: u64,
    tasks: Vec<DeliveryTask>,
    now_millis: u64,
) -> Result<Delivery, CoordinationError> {
    if delivery.revision() != expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before task breakdown approval",
        ));
    }
    if delivery.snapshot().status != DeliveryStatus::Executing {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "DeliveryTask graph requires the current approved plan",
        ));
    }
    if !delivery.snapshot().tasks.is_empty() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "the current Spec revision already has an approved DeliveryTask graph",
        ));
    }
    if tasks.is_empty() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "an approved DeliveryTask graph must contain at least one task",
        ));
    }
    if tasks.iter().any(|task| {
        task.delivery_id != *delivery.id() || task.status != DeliveryTaskStatus::Pending
    }) {
        return Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "approved DeliveryTasks must belong to this Delivery and start pending",
        ));
    }
    let review = delivery
        .snapshot()
        .stage_runs
        .iter()
        .rev()
        .find(|run| run.stage == DeliveryStage::PlanReview)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "DeliveryTask graph has no current plan review",
            )
        })?;
    let approved = review.status == StageRunStatus::Succeeded
        && delivery.snapshot().attention_items.iter().any(|item| {
            item.delivery_spec_id == delivery.snapshot().spec.id
                && item.stage_run_id.as_ref() == Some(&review.id)
                && item.item_type == AttentionItemType::DecisionRequired
                && item.status == AttentionItemStatus::Resolved
        });
    if !approved {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "DeliveryTask graph is not tied to the current approved plan review",
        ));
    }
    let mut snapshot = delivery.clone().into_snapshot();
    snapshot.tasks = tasks;
    snapshot.revision += 1;
    snapshot.updated_at_millis = now_millis;
    Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::InvalidRequest, error.to_string())
    })
}

/// Enforces the new `delivery.create` path before domain construction.
///
/// # Errors
///
/// Rejects every non-empty task list; task proposals enter only through
/// [`approve_task_breakdown`].
pub fn validate_create_tasks_empty(tasks: &[DeliveryTask]) -> Result<(), CoordinationError> {
    if tasks.is_empty() {
        Ok(())
    } else {
        Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "delivery.create.tasks must be empty",
        ))
    }
}

/// Returns the first task in the approved graph that can enter `stage`.
///
/// Task vector order is the approved product order; this function does not
/// schedule Codex Plan items or choose among Agent work.
///
/// # Errors
///
/// Fails when `stage` is Delivery-level or no task has the required state with
/// every dependency completed.
pub fn runnable_task(
    delivery: &Delivery,
    stage: DeliveryStage,
) -> Result<&DeliveryTask, CoordinationError> {
    if !matches!(
        stage,
        DeliveryStage::Executing | DeliveryStage::Verifying | DeliveryStage::Reworking
    ) {
        return Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "Delivery-level stages do not select a DeliveryTask",
        ));
    }
    delivery
        .snapshot()
        .tasks
        .iter()
        .find(|task| {
            let state_matches = match stage {
                DeliveryStage::Executing => task.status == DeliveryTaskStatus::Pending,
                DeliveryStage::Verifying => matches!(
                    task.status,
                    DeliveryTaskStatus::Active | DeliveryTaskStatus::Verifying
                ),
                DeliveryStage::Reworking => task.status == DeliveryTaskStatus::Failed,
                _ => false,
            };
            state_matches
                && task.blocked_by_task_ids.iter().all(|dependency_id| {
                    delivery.snapshot().tasks.iter().any(|dependency| {
                        dependency.id == *dependency_id
                            && dependency.status == DeliveryTaskStatus::Completed
                    })
                })
        })
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "no DeliveryTask is runnable for the next stage",
            )
        })
}
