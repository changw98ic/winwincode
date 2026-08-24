// SPDX-License-Identifier: Apache-2.0

//! Atomic promotion of the task graph sealed by the current solution review.
//!
//! This module is the only production path that turns planner proposals into
//! canonical [`DeliveryTask`] facts. The caller supplies neither tasks nor a
//! mutable Delivery snapshot.

use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};
use winwincode_domain::DeliveryId;

use super::solution_review::ApprovedTaskPromotion;
use crate::domain::{
    DELIVERY_SCHEMA_VERSION, Delivery, DeliverySpecId, DeliveryStatus, DeliveryTask,
    DeliveryTaskStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskBreakdownPromotionErrorCode {
    StaleAuthority,
    WrongState,
    Conflict,
    InvalidGraph,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskBreakdownPromotionError {
    code: TaskBreakdownPromotionErrorCode,
    message: String,
}

impl TaskBreakdownPromotionError {
    pub(crate) const fn code(&self) -> TaskBreakdownPromotionErrorCode {
        self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TaskBreakdownPromotionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for TaskBreakdownPromotionError {}

/// Immutable public event derived from the same sealed transition as the
/// canonical Delivery journal record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryTaskBreakdownApprovedEvent {
    pub schema_version: u8,
    pub delivery_id: DeliveryId,
    pub delivery_revision: u64,
    pub delivery_spec_id: DeliverySpecId,
    pub delivery_spec_revision: u64,
    pub review_set_sha256: String,
    pub tasks: Vec<DeliveryTask>,
}

/// Sealed next Delivery and its exact public event.
///
/// Private fields prevent callers from presenting an edited snapshot as an
/// authorized task promotion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskBreakdownPromotionTransition {
    source_delivery: Delivery,
    source_revision: u64,
    review_set_sha256: String,
    delivery: Delivery,
    event: DeliveryTaskBreakdownApprovedEvent,
}

impl TaskBreakdownPromotionTransition {
    pub(crate) fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    pub(crate) const fn event(&self) -> &DeliveryTaskBreakdownApprovedEvent {
        &self.event
    }

    pub(crate) fn review_set_sha256(&self) -> &str {
        &self.review_set_sha256
    }

    pub(crate) fn validate_source(
        &self,
        delivery: &Delivery,
    ) -> Result<(), TaskBreakdownPromotionError> {
        if delivery != &self.source_delivery || delivery.revision() != self.source_revision {
            return Err(promotion_error(
                TaskBreakdownPromotionErrorCode::StaleAuthority,
                "task-breakdown transition no longer matches its source Delivery",
            ));
        }
        Ok(())
    }
}

/// Promotes the exact ordered proposals from one current approved review seal.
///
/// # Errors
///
/// Rejects a stale or foreign seal, a non-executing Delivery, an existing task
/// graph, or any graph that fails canonical Delivery validation.
pub(crate) fn prepare_task_breakdown_promotion(
    delivery: &Delivery,
    approved: &ApprovedTaskPromotion<'_>,
) -> Result<TaskBreakdownPromotionTransition, TaskBreakdownPromotionError> {
    approved.validate_for_delivery(delivery).map_err(|error| {
        promotion_error(
            TaskBreakdownPromotionErrorCode::StaleAuthority,
            error.to_string(),
        )
    })?;
    if delivery.snapshot().status != DeliveryStatus::Executing {
        return Err(promotion_error(
            TaskBreakdownPromotionErrorCode::WrongState,
            "task breakdown requires the current approved solution review",
        ));
    }
    if !delivery.snapshot().tasks.is_empty() {
        return Err(promotion_error(
            TaskBreakdownPromotionErrorCode::Conflict,
            "the current DeliverySpec already has a promoted task graph",
        ));
    }

    let tasks = approved
        .task_proposals()
        .iter()
        .map(|proposal| DeliveryTask {
            schema_version: DELIVERY_SCHEMA_VERSION,
            id: proposal.id().clone(),
            delivery_id: delivery.id().clone(),
            title: proposal.title().to_owned(),
            goal: proposal.goal().to_owned(),
            acceptance_criterion_ids: proposal.acceptance_criterion_ids().to_vec(),
            blocked_by_task_ids: proposal.blocked_by_task_ids().to_vec(),
            owner: None,
            status: DeliveryTaskStatus::Pending,
        })
        .collect::<Vec<_>>();

    let mut snapshot = delivery.clone().into_snapshot();
    snapshot.tasks = tasks;
    snapshot.revision = snapshot.revision.checked_add(1).ok_or_else(|| {
        promotion_error(
            TaskBreakdownPromotionErrorCode::InvalidGraph,
            "task-breakdown revision overflowed",
        )
    })?;
    let promoted = Delivery::try_from_snapshot(snapshot).map_err(|error| {
        promotion_error(
            TaskBreakdownPromotionErrorCode::InvalidGraph,
            error.to_string(),
        )
    })?;
    let event = DeliveryTaskBreakdownApprovedEvent {
        schema_version: DELIVERY_SCHEMA_VERSION,
        delivery_id: promoted.id().clone(),
        delivery_revision: promoted.revision(),
        delivery_spec_id: promoted.snapshot().spec.id.clone(),
        delivery_spec_revision: promoted.snapshot().spec.revision,
        review_set_sha256: approved.review_set_sha256().to_owned(),
        tasks: promoted.snapshot().tasks.clone(),
    };

    Ok(TaskBreakdownPromotionTransition {
        source_delivery: delivery.clone(),
        source_revision: delivery.revision(),
        review_set_sha256: approved.review_set_sha256().to_owned(),
        delivery: promoted,
        event,
    })
}

/// Rebuilds the immutable event for a verified historical task-promotion
/// journal record. This is used only for request replay; it accepts no caller
/// task data.
pub(crate) fn restore_task_breakdown_event(
    source: &Delivery,
    promoted: &Delivery,
    review_set_sha256: &str,
) -> Result<DeliveryTaskBreakdownApprovedEvent, TaskBreakdownPromotionError> {
    let review = super::solution_review::resolve_current_solution_review(source)
        .map_err(|error| {
            promotion_error(
                TaskBreakdownPromotionErrorCode::StaleAuthority,
                error.to_string(),
            )
        })?
        .ok_or_else(|| {
            promotion_error(
                TaskBreakdownPromotionErrorCode::StaleAuthority,
                "historical task promotion has no solution review",
            )
        })?;
    let approved = review.approved_task_promotion().ok_or_else(|| {
        promotion_error(
            TaskBreakdownPromotionErrorCode::StaleAuthority,
            "historical task promotion was not approved",
        )
    })?;
    if approved.review_set_sha256() != review_set_sha256 {
        return Err(promotion_error(
            TaskBreakdownPromotionErrorCode::StaleAuthority,
            "historical task promotion digest does not match the request",
        ));
    }
    let expected = prepare_task_breakdown_promotion(source, &approved)?;
    if expected.delivery() != promoted {
        return Err(promotion_error(
            TaskBreakdownPromotionErrorCode::StaleAuthority,
            "historical task promotion snapshot was changed",
        ));
    }
    Ok(expected.event().clone())
}

fn promotion_error(
    code: TaskBreakdownPromotionErrorCode,
    message: impl Into<String>,
) -> TaskBreakdownPromotionError {
    TaskBreakdownPromotionError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winwincode_domain::DeliveryTaskId;

    use crate::application::solution_review::{
        resolve_current_solution_review,
        tests::{
            ReviewFixtureState, duplicate_task_and_criterion_fixtures,
            empty_task_proposals_fixture, invalid_dependency_fixtures, review_delivery,
            with_newer_review_attempt,
        },
    };

    #[test]
    fn approved_task_promotion_maps_exact_ordered_proposals() {
        let delivery = review_delivery(ReviewFixtureState::Approved);
        let review = resolve_current_solution_review(&delivery)
            .expect("current review")
            .expect("solution review");
        let approved = review
            .approved_task_promotion()
            .expect("approved_task_promotion");
        let task_proposals = approved.task_proposals().to_vec();
        let transition = prepare_task_breakdown_promotion(&delivery, &approved)
            .expect("prepare_task_breakdown_promotion");

        assert_eq!(
            transition
                .delivery()
                .snapshot()
                .tasks
                .iter()
                .map(|task| task.id.clone())
                .collect::<Vec<DeliveryTaskId>>(),
            task_proposals
                .iter()
                .map(|proposal| proposal.id().clone())
                .collect::<Vec<_>>()
        );
        for (task, proposal) in transition
            .delivery()
            .snapshot()
            .tasks
            .iter()
            .zip(task_proposals.iter())
        {
            assert_eq!(task.title, proposal.title());
            assert_eq!(task.goal, proposal.goal());
            assert_eq!(
                task.acceptance_criterion_ids,
                proposal.acceptance_criterion_ids()
            );
            assert_eq!(task.blocked_by_task_ids, proposal.blocked_by_task_ids());
            assert_eq!(task.owner, None);
            assert_eq!(task.status, DeliveryTaskStatus::Pending);
        }
    }

    #[test]
    fn task_breakdown_transition_rejects_changed_source_or_seal() {
        let delivery = review_delivery(ReviewFixtureState::Approved);
        let review = resolve_current_solution_review(&delivery)
            .expect("current review")
            .expect("solution review");
        let approved = review
            .approved_task_promotion()
            .expect("approved promotion");
        let review_set_sha256 = approved.review_set_sha256().to_owned();
        let transition = prepare_task_breakdown_promotion(&delivery, &approved)
            .expect("prepare_task_breakdown_promotion");
        transition
            .validate_source(&delivery)
            .expect("validate_for_delivery and validate_source");

        let changed = with_newer_review_attempt(delivery.clone(), ReviewFixtureState::Pending);
        let rejected = transition.validate_source(&changed);
        assert!(rejected.is_err());
        assert_eq!(transition.review_set_sha256(), review_set_sha256);
        assert!(approved.validate_for_delivery(&changed).is_err());
    }

    #[test]
    fn solution_review_rejects_empty_task_proposals() {
        let _wire_field = "taskProposals";
        let empty = empty_task_proposals_fixture();
        assert!(resolve_current_solution_review(&empty).is_err());
    }

    #[test]
    fn solution_review_rejects_duplicate_task_and_criterion_ids() {
        let _wire_fields = ("taskProposals", "acceptanceCriterionIds");
        let (duplicate, duplicate_criterion) = duplicate_task_and_criterion_fixtures();
        assert!(resolve_current_solution_review(&duplicate).is_err());
        assert!(resolve_current_solution_review(&duplicate_criterion).is_err());
    }

    #[test]
    fn solution_review_rejects_self_missing_duplicate_and_cyclic_dependencies() {
        let _wire_field = "blockedByTaskIds";
        let [self_dependency, missing, duplicate, cycle] = invalid_dependency_fixtures();
        assert!(resolve_current_solution_review(&self_dependency).is_err());
        assert!(resolve_current_solution_review(&missing).is_err());
        assert!(resolve_current_solution_review(&duplicate).is_err());
        assert!(resolve_current_solution_review(&cycle).is_err());
    }
}
