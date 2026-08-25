// SPDX-License-Identifier: Apache-2.0

//! Approved `DeliveryTask` graph transitions.

use crate::domain::{Delivery, DeliveryStage, DeliveryTask, DeliveryTaskStatus};

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
        (DeliveryTaskStatus::Pending, TaskFact::StartExecuting)
        | (DeliveryTaskStatus::Failed, TaskFact::StartReworking) => DeliveryTaskStatus::Active,
        (DeliveryTaskStatus::Active | DeliveryTaskStatus::Verifying, TaskFact::StartVerifying)
        | (DeliveryTaskStatus::Verifying, TaskFact::VerifyingCancelled) => {
            DeliveryTaskStatus::Verifying
        }
        (DeliveryTaskStatus::Verifying, TaskFact::VerificationPassed) => {
            DeliveryTaskStatus::Completed
        }
        (DeliveryTaskStatus::Verifying, TaskFact::VerificationFailed)
        | (DeliveryTaskStatus::Active, TaskFact::ReworkingCancelled) => DeliveryTaskStatus::Failed,
        (DeliveryTaskStatus::Active, TaskFact::ExecutingCancelled) => DeliveryTaskStatus::Pending,
        _ => {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "DeliveryTask status does not accept this Delivery fact",
            ));
        }
    };
    Ok(next)
}

/// Enforces the new `delivery.create` path before domain construction.
///
/// # Errors
///
/// Rejects every non-empty task list; task proposals enter only through
/// [`super::task_breakdown::prepare_task_breakdown_promotion`].
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
