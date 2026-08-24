// SPDX-License-Identifier: Apache-2.0

//! Business Attention transitions.

use winwincode_domain::{AttentionItemId, StageRunId};

use crate::domain::{
    AttentionItemStatus, AttentionItemType, Delivery, DeliveryStage, DeliveryStatus,
    StageRunActorType, StageRunStatus,
};

use super::{CoordinationError, CoordinationErrorCode};

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
    let next_status = next_delivery_status(item.item_type, run.stage, input.decision)?;
    let mut snapshot = delivery.clone().into_snapshot();
    let stored_item = &mut snapshot.attention_items[item_index];
    stored_item.status = match input.decision {
        AttentionDecision::Resolved => AttentionItemStatus::Resolved,
        AttentionDecision::Dismissed => AttentionItemStatus::Dismissed,
    };
    stored_item.resolution = Some(input.resolution);
    stored_item.resolved_by = Some(input.actor);
    stored_item.resolved_at_millis = Some(input.now_millis);
    if run.actor_type == StageRunActorType::Human {
        let stored_run = &mut snapshot.stage_runs[run_index];
        stored_run.status = match input.decision {
            AttentionDecision::Resolved => StageRunStatus::Succeeded,
            AttentionDecision::Dismissed => StageRunStatus::Failed,
        };
        stored_run.finished_at_millis = Some(input.now_millis);
    }
    snapshot.status = if snapshot
        .attention_items
        .iter()
        .any(|item| item.blocking && item.status == AttentionItemStatus::Open)
    {
        DeliveryStatus::NeedsAttention
    } else {
        next_status
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
