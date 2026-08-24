// SPDX-License-Identifier: Apache-2.0

//! Business Attention transitions.

use winwincode_domain::{AttentionItemId, StageRunId};

use crate::domain::{
    AttentionItemStatus, AttentionItemType, Delivery, DeliverySnapshot, DeliveryStage,
    DeliveryStatus, StageRunActorType, StageRunStatus,
    rework::{resolved_verdict_attention_action, safest_attention_transition},
};

use super::{
    CoordinationError, CoordinationErrorCode, require_mutation_time,
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
    pub stage_run_id: StageRunId,
    pub expected_context: String,
    pub actor: String,
    pub decision: AttentionDecision,
    pub resolution: String,
    pub now_millis: u64,
}

/// Resolves one current business Attention item without starting execution.
///
/// # Errors
///
/// Fails closed on stale revision, actor, item, `StageRun`, `Spec`, frozen context,
/// time, or decision state. No snapshot is returned on error.
pub fn resolve_attention(
    delivery: &Delivery,
    input: ResolveAttentionInput,
) -> Result<Delivery, CoordinationError> {
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
        || item.stage_run_id.as_ref() != Some(&input.stage_run_id)
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
            "Attention resolution does not match the current actor, StageRun, Spec, or frozen context",
        ));
    }
    if item.blocking && delivery.snapshot().status != DeliveryStatus::NeedsAttention {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "blocking Attention can be resolved only while Delivery needs attention",
        ));
    }
    let run_index = delivery
        .snapshot()
        .stage_runs
        .iter()
        .position(|run| run.id == input.stage_run_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::StaleAttention,
                "Attention StageRun is no longer current",
            )
        })?;
    let run = &delivery.snapshot().stage_runs[run_index];
    if run.actor_type == StageRunActorType::Human
        && (!matches!(
            run.status,
            StageRunStatus::Waiting | StageRunStatus::Running
        ) || input.now_millis < run.started_at_millis)
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::StaleAttention,
            "human review StageRun is no longer waiting for this Attention decision",
        ));
    }
    let verdict_actions = current_verdict_attention_actions(delivery, item)?;
    if verdict_actions.is_some() && input.decision != AttentionDecision::Resolved {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "computed verdict Attention must be resolved before stage movement",
        ));
    }
    apply_resolution(
        delivery.clone().into_snapshot(),
        input,
        item_index,
        run_index,
        verdict_actions,
    )
}

fn apply_resolution(
    mut snapshot: DeliverySnapshot,
    input: ResolveAttentionInput,
    item_index: usize,
    run_index: usize,
    verdict_actions: Option<Vec<crate::domain::rework::VerdictAttentionAction>>,
) -> Result<Delivery, CoordinationError> {
    let item_type = snapshot.attention_items[item_index].item_type;
    let run_stage = snapshot.stage_runs[run_index].stage;
    let run_actor_type = snapshot.stage_runs[run_index].actor_type;
    let stored_item = &mut snapshot.attention_items[item_index];
    stored_item.status = match input.decision {
        AttentionDecision::Resolved => AttentionItemStatus::Resolved,
        AttentionDecision::Dismissed => AttentionItemStatus::Dismissed,
    };
    stored_item.resolution = Some(input.resolution);
    stored_item.resolved_by = Some(input.actor);
    stored_item.resolved_at_millis = Some(input.now_millis);

    let linked_review_still_open = snapshot.attention_items.iter().any(|item| {
        item.stage_run_id.as_ref() == Some(&input.stage_run_id)
            && item.blocking
            && item.status == AttentionItemStatus::Open
    });
    let review_decision = if run_actor_type == StageRunActorType::Human && !linked_review_still_open
    {
        let decision = if snapshot.attention_items.iter().any(|item| {
            item.stage_run_id.as_ref() == Some(&input.stage_run_id)
                && item.status == AttentionItemStatus::Dismissed
        }) {
            AttentionDecision::Dismissed
        } else {
            AttentionDecision::Resolved
        };
        let stored_run = &mut snapshot.stage_runs[run_index];
        stored_run.status = match decision {
            AttentionDecision::Resolved => StageRunStatus::Succeeded,
            AttentionDecision::Dismissed => StageRunStatus::Failed,
        };
        stored_run.finished_at_millis = Some(input.now_millis);
        decision
    } else {
        input.decision
    };
    snapshot.status = if snapshot
        .attention_items
        .iter()
        .any(|item| item.blocking && item.status == AttentionItemStatus::Open)
    {
        DeliveryStatus::NeedsAttention
    } else {
        let actions = verdict_actions.unwrap_or_else(|| {
            snapshot
                .attention_items
                .iter()
                .filter(|item| {
                    item.blocking
                        && item.stage_run_id.as_ref() == Some(&input.stage_run_id)
                        && item.delivery_spec_id == snapshot.spec.id
                })
                .filter_map(|item| resolved_verdict_attention_action(item.item_type, item.status))
                .collect()
        });
        if actions.is_empty() {
            next_delivery_status(item_type, run_stage, review_decision)?
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
    stage: DeliveryStage,
    decision: AttentionDecision,
) -> Result<DeliveryStatus, CoordinationError> {
    let status = match (item_type, stage, decision) {
        (
            AttentionItemType::DecisionRequired,
            DeliveryStage::PlanReview,
            AttentionDecision::Resolved,
        ) => DeliveryStatus::Executing,
        (
            AttentionItemType::DecisionRequired,
            DeliveryStage::PlanReview,
            AttentionDecision::Dismissed,
        ) => DeliveryStatus::Planning,
        (
            AttentionItemType::DeliveryApproval,
            DeliveryStage::DeliveryReview,
            AttentionDecision::Resolved,
        ) => DeliveryStatus::Delivered,
        (
            AttentionItemType::DeliveryApproval,
            DeliveryStage::DeliveryReview,
            AttentionDecision::Dismissed,
        ) => DeliveryStatus::Reworking,
        (AttentionItemType::RequirementQuestion, _, AttentionDecision::Resolved) => {
            DeliveryStatus::Ready
        }
        (AttentionItemType::RequirementQuestion, _, AttentionDecision::Dismissed)
        | (AttentionItemType::ScopeChange, _, _) => DeliveryStatus::Clarifying,
        (AttentionItemType::VerificationBlocked, _, AttentionDecision::Resolved) => {
            DeliveryStatus::Verifying
        }
        (AttentionItemType::VerificationBlocked, _, AttentionDecision::Dismissed) => {
            DeliveryStatus::Reworking
        }
        _ => {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "Attention type is not actionable for its linked StageRun",
            ));
        }
    };
    Ok(status)
}
