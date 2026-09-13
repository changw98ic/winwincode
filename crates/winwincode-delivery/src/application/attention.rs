// SPDX-License-Identifier: Apache-2.0

//! Business Attention transitions.

use std::ops::Deref;

use winwincode_domain::{AttentionItemId, WorkRunId};

use crate::domain::{
    AttentionItem, AttentionItemStatus, AttentionItemType, Delivery, DeliverySnapshot,
    DeliveryStatus,
    rework::{resolved_verdict_attention_action, safest_attention_transition},
};

use super::{
    CoordinationError, CoordinationErrorCode, require_mutation_time,
    solution_review::{
        SolutionReviewErrorCode, ValidatedSolutionReviewSettlement,
        resolve_current_solution_review, validate_solution_review_settlement,
    },
    verdict::current_verdict_attention_actions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionDecision {
    Resolved,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveAttentionInput {
    pub expected_revision: u64,
    pub attention_item_id: AttentionItemId,
    /// Canonical machine run that produced the item. Human review identity is
    /// recovered from the validated solution-review context.
    pub work_run_id: Option<WorkRunId>,
    pub expected_context: String,
    pub actor: String,
    pub decision: AttentionDecision,
    pub resolution: String,
    pub now_millis: u64,
}

/// One application-owned Attention resolution ready for its dedicated journal command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAttentionTransition {
    source_delivery: Delivery,
    delivery: Delivery,
}

impl ResolvedAttentionTransition {
    #[must_use]
    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    #[must_use]
    pub fn into_delivery(self) -> Delivery {
        self.delivery
    }

    pub(crate) fn validate_source(&self, current: &Delivery) -> Result<(), CoordinationError> {
        if self.source_delivery != *current {
            return Err(CoordinationError::new(
                CoordinationErrorCode::RevisionConflict,
                "Attention resolution source is not the exact current Delivery",
            ));
        }
        if self.delivery.id() != current.id()
            || self.delivery.revision() != current.revision().saturating_add(1)
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "Attention resolution is not the next revision of its source Delivery",
            ));
        }
        Ok(())
    }
}

impl Deref for ResolvedAttentionTransition {
    type Target = Delivery;

    fn deref(&self) -> &Self::Target {
        &self.delivery
    }
}

/// Resolves one current business Attention item without starting execution.
///
/// # Errors
///
/// Fails closed on stale revision, actor, item, `WorkRun`, `Spec`, frozen context,
/// time, or decision state. No snapshot is returned on error.
///
/// # Panics
/// This transition does not intentionally panic; all indexed data is checked
/// before use.
pub fn resolve_attention(
    delivery: &Delivery,
    input: ResolveAttentionInput,
) -> Result<ResolvedAttentionTransition, CoordinationError> {
    if delivery.revision() != input.expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before Attention resolution",
        ));
    }
    require_mutation_time(delivery, input.now_millis)?;
    let item_index = delivery
        .snapshot()
        .attention_items
        .iter()
        .position(|item| item.id == input.attention_item_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::StaleAttention,
                "AttentionItem does not belong to the current Delivery",
            )
        })?;
    let item = &delivery.snapshot().attention_items[item_index];
    if item.delivery_id != *delivery.id()
        || item.delivery_spec_id != delivery.snapshot().spec.id
        || item.status != AttentionItemStatus::Open
        || item.context != input.expected_context
        || item
            .assigned_to
            .as_ref()
            .is_some_and(|assigned| assigned != &input.actor)
        || input.now_millis < item.created_at_millis
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::StaleAttention,
            "Attention resolution does not match the current actor, run, Spec, or frozen context",
        ));
    }
    if input.work_run_id.as_ref() != item.work_run_id.as_ref() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::StaleAttention,
            "Attention resolution does not match its canonical WorkRun",
        ));
    }
    if item.blocking && delivery.snapshot().status != DeliveryStatus::NeedsAttention {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "blocking Attention can be resolved only while Delivery needs attention",
        ));
    }
    if item.item_type == AttentionItemType::DeliveryApproval {
        let verdict = delivery
            .snapshot()
            .verdict
            .as_ref()
            .filter(|verdict| verdict.status == crate::domain::CriterionVerdict::Pass)
            .ok_or_else(|| {
                CoordinationError::new(
                    CoordinationErrorCode::StaleAttention,
                    "delivery approval requires the current passing verdict",
                )
            })?;
        let producer_work_run_id = item.work_run_id.as_ref().ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::StaleAttention,
                "delivery approval has no verified candidate WorkRun",
            )
        })?;
        let expected = super::verdict::delivery_approval(
            delivery.snapshot(),
            verdict,
            producer_work_run_id,
            item.created_at_millis,
        )?;
        if item.id != expected.id
            || item.context != expected.context
            || item.work_run_id != expected.work_run_id
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::StaleAttention,
                "delivery approval no longer matches the verified candidate",
            ));
        }
    }
    let solution_review_settlement = resolve_typed_review_context(delivery, item, &input)?;
    let verdict_actions = current_verdict_attention_actions(delivery, item)?;
    if verdict_actions.is_some() && input.decision != AttentionDecision::Resolved {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "computed verdict Attention must be resolved before stage movement",
        ));
    }
    let resolved = apply_resolution(
        delivery.clone().into_snapshot(),
        input,
        item_index,
        verdict_actions,
        solution_review_settlement,
    )?;
    Ok(ResolvedAttentionTransition {
        source_delivery: delivery.clone(),
        delivery: resolved,
    })
}

fn resolve_typed_review_context(
    delivery: &Delivery,
    item: &AttentionItem,
    input: &ResolveAttentionInput,
) -> Result<Option<ValidatedSolutionReviewSettlement>, CoordinationError> {
    let review = if item.item_type == AttentionItemType::DecisionRequired {
        resolve_current_solution_review(delivery).map_err(|error| solution_review_error(&error))?
    } else {
        None
    };
    let Some(review) = review else {
        return Ok(None);
    };
    let view = review.projection_view();
    if view.attention_item_id != &item.id
        || item.work_run_id.as_ref() != Some(view.planning_work_run_id)
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::StaleAttention,
            "DecisionRequired Attention is not the current typed solution review",
        ));
    }
    typed_solution_review_settlement(delivery, item, input)
}

fn typed_solution_review_settlement(
    delivery: &Delivery,
    item: &AttentionItem,
    input: &ResolveAttentionInput,
) -> Result<Option<ValidatedSolutionReviewSettlement>, CoordinationError> {
    if item.item_type != AttentionItemType::DecisionRequired {
        return Ok(None);
    }
    let settlement = validate_solution_review_settlement(
        delivery,
        &input.attention_item_id,
        &input.actor,
        &input.resolution,
        input.now_millis,
    )
    .map_err(|error| {
        let code = match error.code() {
            SolutionReviewErrorCode::InvalidEncoding | SolutionReviewErrorCode::InvalidContent => {
                CoordinationErrorCode::InvalidRequest
            }
            SolutionReviewErrorCode::StaleAuthority
            | SolutionReviewErrorCode::AmbiguousCurrentReview => {
                CoordinationErrorCode::StaleAttention
            }
        };
        CoordinationError::new(code, error.message())
    })?;
    let input_resolves = input.decision == AttentionDecision::Resolved;
    if settlement.resolve_attention() != input_resolves {
        return Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "plan-review Attention decision does not match its typed solution-review action",
        ));
    }
    Ok(Some(settlement))
}

fn solution_review_error(error: &super::solution_review::SolutionReviewError) -> CoordinationError {
    let code = match error.code() {
        SolutionReviewErrorCode::InvalidEncoding | SolutionReviewErrorCode::InvalidContent => {
            CoordinationErrorCode::InvalidRequest
        }
        SolutionReviewErrorCode::StaleAuthority
        | SolutionReviewErrorCode::AmbiguousCurrentReview => CoordinationErrorCode::StaleAttention,
    };
    CoordinationError::new(code, error.message())
}

fn apply_resolution(
    mut snapshot: DeliverySnapshot,
    input: ResolveAttentionInput,
    item_index: usize,
    verdict_actions: Option<Vec<crate::domain::rework::VerdictAttentionAction>>,
    solution_review_settlement: Option<ValidatedSolutionReviewSettlement>,
) -> Result<Delivery, CoordinationError> {
    let item_type = snapshot.attention_items[item_index].item_type;
    let target_work_run_id = snapshot.attention_items[item_index].work_run_id.clone();
    let stored_item = &mut snapshot.attention_items[item_index];
    stored_item.status = solution_review_settlement.map_or_else(
        || match input.decision {
            AttentionDecision::Resolved => AttentionItemStatus::Resolved,
            AttentionDecision::Dismissed => AttentionItemStatus::Dismissed,
        },
        ValidatedSolutionReviewSettlement::attention_status,
    );
    stored_item.resolution = Some(input.resolution);
    stored_item.resolved_by = Some(input.actor);
    stored_item.resolved_at_millis = Some(input.now_millis);

    if item_type == AttentionItemType::DeliveryApproval
        && input.decision == AttentionDecision::Resolved
    {
        let work_run_id = target_work_run_id.as_ref().ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::StaleAttention,
                "delivery approval has no verified candidate WorkRun",
            )
        })?;
        snapshot
            .work_run_aggregate
            .complete_candidate(work_run_id)
            .map_err(|_| {
                CoordinationError::new(
                    CoordinationErrorCode::StaleAttention,
                    "delivery approval candidate is no longer ready for completion",
                )
            })?;
    }

    let review_decision = input.decision;
    snapshot.status = if snapshot
        .attention_items
        .iter()
        .any(|item| item.blocking && item.status == AttentionItemStatus::Open)
    {
        DeliveryStatus::NeedsAttention
    } else if item_type == AttentionItemType::DeliveryApproval
        && input.decision == AttentionDecision::Resolved
        && snapshot
            .work_run_aggregate
            .items
            .iter()
            .any(|item| item.state != winwincode_domain::WorkItemState::Done)
    {
        snapshot.verdict = None;
        DeliveryStatus::Ready
    } else if let Some(settlement) = solution_review_settlement {
        settlement.delivery_status()
    } else {
        let actions = verdict_actions.unwrap_or_else(|| {
            snapshot
                .attention_items
                .iter()
                .filter(|item| {
                    item.blocking
                        && target_work_run_id.is_some()
                        && item.work_run_id == target_work_run_id
                        && item.delivery_spec_id == snapshot.spec.id
                })
                .filter_map(|item| resolved_verdict_attention_action(item.item_type, item.status))
                .collect()
        });
        if actions.is_empty() {
            next_delivery_status(item_type, review_decision)?
        } else {
            safest_attention_transition(&actions)
        }
    };
    snapshot.revision += 1;
    snapshot.updated_at_millis = input.now_millis;
    Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::StaleAttention, error.to_string())
    })
}

fn next_delivery_status(
    item_type: AttentionItemType,
    decision: AttentionDecision,
) -> Result<DeliveryStatus, CoordinationError> {
    let status = match (item_type, decision) {
        (AttentionItemType::DeliveryApproval, AttentionDecision::Resolved) => {
            DeliveryStatus::Delivered
        }
        (
            AttentionItemType::RequirementQuestion | AttentionItemType::VerificationBlocked,
            AttentionDecision::Resolved,
        ) => DeliveryStatus::Ready,
        (AttentionItemType::RequirementQuestion, AttentionDecision::Dismissed)
        | (AttentionItemType::ScopeChange, _) => DeliveryStatus::Clarifying,
        (
            AttentionItemType::DeliveryApproval | AttentionItemType::VerificationBlocked,
            AttentionDecision::Dismissed,
        ) => DeliveryStatus::Reworking,
        _ => {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "Attention type is not actionable for its linked WorkRun",
            ));
        }
    };
    Ok(status)
}
