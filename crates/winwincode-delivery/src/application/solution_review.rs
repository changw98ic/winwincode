// SPDX-License-Identifier: Apache-2.0

//! Typed solution-review authority reconstructed from the current Delivery.
//!
//! Raw Attention JSON is accepted only by [`resolve_current_solution_review`].
//! Callers receive neither a raw parser nor a constructor for the validated
//! fact. The resolver verifies the current Delivery identities, the exact
//! planning binding, the human review lifecycle, the ordered task graph, and
//! the canonical review-set digest before producing one opaque value.
//!
//! ```compile_fail
//! use winwincode_delivery::application::solution_review::ValidatedSolutionReviewSet;
//!
//! let _caller_supplied: ValidatedSolutionReviewSet = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::solution_review::SolutionReviewContextV1;
//!
//! let _caller_context: SolutionReviewContextV1 = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::solution_review::SolutionReviewDecisionV1;
//!
//! let _caller_decision: SolutionReviewDecisionV1 = serde_json::from_str("{}").unwrap();
//! ```

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{AttentionItemId, DeliveryId, DeliveryTaskId, StageRunId};

use crate::domain::{
    AcceptanceCriterionId, AttentionItem, AttentionItemStatus, AttentionItemType, Delivery,
    DeliverySpecId, DeliveryStage, MAX_SAFE_INTEGER, SessionBindingId, StageRun, StageRunActorType,
    StageRunStatus,
};

const SOLUTION_REVIEW_SCHEMA_VERSION: u8 = 1;
const SOLUTION_REVIEW_CONTEXT_PROTOCOL: &str = "winwincode.solution-review-context.v1";
const SOLUTION_REVIEW_DECISION_PROTOCOL: &str = "winwincode.solution-review-decision.v1";
const MAX_TEXT_CODE_UNITS: usize = 65_536;
const MAX_TITLE_CODE_UNITS: usize = 256;
const MAX_COLLECTION_ITEMS: usize = 200;
const MAX_REPOSITORY_PATH_LENGTH: usize = 4_096;
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const PLATFORM_NODE_IDS: [&str; 4] = [
    "platform:dsh",
    "platform:strongflow",
    "platform:codex-core",
    "platform:repository",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SolutionReviewErrorCode {
    InvalidEncoding,
    InvalidContent,
    StaleAuthority,
    AmbiguousCurrentReview,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SolutionReviewError {
    code: SolutionReviewErrorCode,
    message: String,
}

impl SolutionReviewError {
    pub(crate) const fn code(&self) -> SolutionReviewErrorCode {
        self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SolutionReviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for SolutionReviewError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidatedReviewStatus {
    Pending,
    Approved,
    ChangesRequested,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidatedReviewDecision {
    Approve,
    RequestChanges,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ValidatedSolutionComponentKind {
    #[serde(rename = "component")]
    Component,
    #[serde(rename = "external")]
    External,
    #[serde(rename = "data-store")]
    DataStore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ValidatedDiagramKind {
    #[serde(rename = "system-architecture")]
    SystemArchitecture,
    #[serde(rename = "process-flow")]
    ProcessFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ValidatedDiagramNodeKind {
    #[serde(rename = "interaction")]
    Interaction,
    #[serde(rename = "delivery-control")]
    DeliveryControl,
    #[serde(rename = "execution")]
    Execution,
    #[serde(rename = "repository")]
    Repository,
    #[serde(rename = "component")]
    Component,
    #[serde(rename = "external")]
    External,
    #[serde(rename = "data-store")]
    DataStore,
    #[serde(rename = "stage")]
    Stage,
    #[serde(rename = "decision")]
    Decision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ValidatedSolutionComponent {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) responsibility: String,
    pub(crate) kind: ValidatedSolutionComponentKind,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) trust_boundary: Option<String>,
    pub(crate) unresolved: bool,
    pub(crate) repository_path_prefixes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ValidatedSolutionConnection {
    pub(crate) id: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SolutionWire {
    id: String,
    summary: String,
    approach: Vec<String>,
    components: Vec<ValidatedSolutionComponent>,
    connections: Vec<ValidatedSolutionConnection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ValidatedDiagramNode {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) kind: ValidatedDiagramNodeKind,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) trust_boundary: Option<String>,
    pub(crate) unresolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ValidatedDiagramEdge {
    pub(crate) id: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ValidatedDiagram {
    pub(crate) id: String,
    pub(crate) kind: ValidatedDiagramKind,
    pub(crate) title: String,
    pub(crate) nodes: Vec<ValidatedDiagramNode>,
    pub(crate) edges: Vec<ValidatedDiagramEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeliveryTaskProposal {
    id: DeliveryTaskId,
    title: String,
    goal: String,
    acceptance_criterion_ids: Vec<AcceptanceCriterionId>,
    blocked_by_task_ids: Vec<DeliveryTaskId>,
}

impl DeliveryTaskProposal {
    pub(crate) fn id(&self) -> &DeliveryTaskId {
        &self.id
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn goal(&self) -> &str {
        &self.goal
    }

    pub(crate) fn acceptance_criterion_ids(&self) -> &[AcceptanceCriterionId] {
        &self.acceptance_criterion_ids
    }

    pub(crate) fn blocked_by_task_ids(&self) -> &[DeliveryTaskId] {
        &self.blocked_by_task_ids
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SolutionReviewContextV1 {
    schema_version: u8,
    protocol: String,
    delivery_id: DeliveryId,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    planning_stage_run_id: StageRunId,
    planning_session_binding_id: SessionBindingId,
    review_stage_run_id: StageRunId,
    attention_item_id: AttentionItemId,
    solution: SolutionWire,
    architecture_diagram: ValidatedDiagram,
    process_diagram: ValidatedDiagram,
    risks: Vec<String>,
    unresolved_items: Vec<String>,
    task_proposals: Vec<DeliveryTaskProposal>,
    prepared_at: u64,
    review_set_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum DecisionActionWire {
    #[serde(rename = "approve")]
    Approve,
    #[serde(rename = "request_changes")]
    RequestChanges,
    #[serde(rename = "reject")]
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SolutionReviewDecisionV1 {
    schema_version: u8,
    protocol: String,
    delivery_id: DeliveryId,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    review_stage_run_id: StageRunId,
    attention_item_id: AttentionItemId,
    review_set_sha256: String,
    action: DecisionActionWire,
    #[serde(deserialize_with = "deserialize_required_option")]
    comments: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    requested_changes: Option<Vec<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewDigestInput<'context> {
    schema_version: u8,
    protocol: &'context str,
    delivery_id: &'context DeliveryId,
    delivery_spec_id: &'context DeliverySpecId,
    delivery_spec_revision: u64,
    planning_stage_run_id: &'context StageRunId,
    planning_session_binding_id: &'context SessionBindingId,
    review_stage_run_id: &'context StageRunId,
    attention_item_id: &'context AttentionItemId,
    solution: &'context SolutionWire,
    architecture_diagram: &'context ValidatedDiagram,
    process_diagram: &'context ValidatedDiagram,
    risks: &'context [String],
    unresolved_items: &'context [String],
    task_proposals: &'context [DeliveryTaskProposal],
    prepared_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedSolutionReviewSet {
    delivery_id: DeliveryId,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    planning_stage_run_id: StageRunId,
    planning_session_binding_id: SessionBindingId,
    review_stage_run_id: StageRunId,
    attention_item_id: AttentionItemId,
    review_set_sha256: String,
    solution_id: String,
    summary: String,
    approach: Vec<String>,
    components: Vec<ValidatedSolutionComponent>,
    connections: Vec<ValidatedSolutionConnection>,
    architecture_diagram: ValidatedDiagram,
    process_diagram: ValidatedDiagram,
    risks: Vec<String>,
    unresolved_items: Vec<String>,
    task_proposals: Vec<DeliveryTaskProposal>,
    prepared_at: u64,
    review_status: ValidatedReviewStatus,
    decision: Option<ValidatedReviewDecision>,
    comments: Option<String>,
    requested_changes: Option<Vec<String>>,
    reviewer_id: Option<String>,
    reviewed_at: Option<u64>,
}

pub(crate) struct SolutionReviewView<'review> {
    pub(crate) delivery_id: &'review DeliveryId,
    pub(crate) delivery_spec_id: &'review DeliverySpecId,
    pub(crate) delivery_spec_revision: u64,
    pub(crate) planning_stage_run_id: &'review StageRunId,
    pub(crate) planning_session_binding_id: &'review SessionBindingId,
    pub(crate) review_stage_run_id: &'review StageRunId,
    pub(crate) attention_item_id: &'review AttentionItemId,
    pub(crate) review_set_sha256: &'review str,
    pub(crate) solution_id: &'review str,
    pub(crate) summary: &'review str,
    pub(crate) approach: &'review [String],
    pub(crate) components: &'review [ValidatedSolutionComponent],
    pub(crate) connections: &'review [ValidatedSolutionConnection],
    pub(crate) architecture_diagram: &'review ValidatedDiagram,
    pub(crate) process_diagram: &'review ValidatedDiagram,
    pub(crate) risks: &'review [String],
    pub(crate) unresolved_items: &'review [String],
    pub(crate) task_proposals: &'review [DeliveryTaskProposal],
    pub(crate) review_status: ValidatedReviewStatus,
    pub(crate) decision: Option<ValidatedReviewDecision>,
    pub(crate) comments: Option<&'review str>,
    pub(crate) requested_changes: Option<&'review [String]>,
    pub(crate) reviewer_id: Option<&'review str>,
    pub(crate) reviewed_at: Option<u64>,
}

#[allow(dead_code)] // Consumed by the phase 2.5.7 sealed task-promotion transition.
pub(crate) struct ApprovedTaskPromotion<'review> {
    delivery_id: &'review DeliveryId,
    delivery_spec_id: &'review DeliverySpecId,
    delivery_spec_revision: u64,
    planning_stage_run_id: &'review StageRunId,
    planning_session_binding_id: &'review SessionBindingId,
    review_stage_run_id: &'review StageRunId,
    attention_item_id: &'review AttentionItemId,
    reviewer_id: &'review str,
    reviewed_at: u64,
    review_set_sha256: &'review str,
    task_proposals: &'review [DeliveryTaskProposal],
}

#[allow(dead_code)] // Kept narrow until the phase 2.5.7 consumer is merged.
impl ApprovedTaskPromotion<'_> {
    pub(crate) fn review_set_sha256(&self) -> &str {
        self.review_set_sha256
    }

    pub(crate) fn task_proposals(&self) -> &[DeliveryTaskProposal] {
        self.task_proposals
    }

    pub(crate) fn validate_for_delivery(
        &self,
        delivery: &Delivery,
    ) -> Result<(), SolutionReviewError> {
        let current = resolve_current_solution_review(delivery)?
            .ok_or_else(|| stale_authority("approved task promotion has no current review"))?;
        if current.review_status != ValidatedReviewStatus::Approved
            || &current.delivery_id != self.delivery_id
            || &current.delivery_spec_id != self.delivery_spec_id
            || current.delivery_spec_revision != self.delivery_spec_revision
            || &current.planning_stage_run_id != self.planning_stage_run_id
            || &current.planning_session_binding_id != self.planning_session_binding_id
            || &current.review_stage_run_id != self.review_stage_run_id
            || &current.attention_item_id != self.attention_item_id
            || current.reviewer_id.as_deref() != Some(self.reviewer_id)
            || current.reviewed_at != Some(self.reviewed_at)
            || current.review_set_sha256 != self.review_set_sha256
            || current.task_proposals.as_slice() != self.task_proposals
        {
            return Err(stale_authority(
                "approved task promotion is not the exact current review attempt",
            ));
        }
        Ok(())
    }
}

impl ValidatedSolutionReviewSet {
    pub(crate) fn review_set_sha256(&self) -> &str {
        &self.review_set_sha256
    }

    pub(crate) fn projection_view(&self) -> SolutionReviewView<'_> {
        SolutionReviewView {
            delivery_id: &self.delivery_id,
            delivery_spec_id: &self.delivery_spec_id,
            delivery_spec_revision: self.delivery_spec_revision,
            planning_stage_run_id: &self.planning_stage_run_id,
            planning_session_binding_id: &self.planning_session_binding_id,
            review_stage_run_id: &self.review_stage_run_id,
            attention_item_id: &self.attention_item_id,
            review_set_sha256: &self.review_set_sha256,
            solution_id: &self.solution_id,
            summary: &self.summary,
            approach: &self.approach,
            components: &self.components,
            connections: &self.connections,
            architecture_diagram: &self.architecture_diagram,
            process_diagram: &self.process_diagram,
            risks: &self.risks,
            unresolved_items: &self.unresolved_items,
            task_proposals: &self.task_proposals,
            review_status: self.review_status,
            decision: self.decision,
            comments: self.comments.as_deref(),
            requested_changes: self.requested_changes.as_deref(),
            reviewer_id: self.reviewer_id.as_deref(),
            reviewed_at: self.reviewed_at,
        }
    }

    #[allow(dead_code)] // The only authority seam reserved for phase 2.5.7.
    pub(crate) fn approved_task_promotion(&self) -> Option<ApprovedTaskPromotion<'_>> {
        if self.review_status != ValidatedReviewStatus::Approved {
            return None;
        }
        Some(ApprovedTaskPromotion {
            delivery_id: &self.delivery_id,
            delivery_spec_id: &self.delivery_spec_id,
            delivery_spec_revision: self.delivery_spec_revision,
            planning_stage_run_id: &self.planning_stage_run_id,
            planning_session_binding_id: &self.planning_session_binding_id,
            review_stage_run_id: &self.review_stage_run_id,
            attention_item_id: &self.attention_item_id,
            reviewer_id: self.reviewer_id.as_deref()?,
            reviewed_at: self.reviewed_at?,
            review_set_sha256: &self.review_set_sha256,
            task_proposals: &self.task_proposals,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedSolutionReviewSettlement {
    resolve_attention: bool,
    attention_status: AttentionItemStatus,
    stage_status: StageRunStatus,
    delivery_status: crate::domain::DeliveryStatus,
}

impl ValidatedSolutionReviewSettlement {
    pub(crate) const fn resolve_attention(self) -> bool {
        self.resolve_attention
    }

    pub(crate) const fn attention_status(self) -> AttentionItemStatus {
        self.attention_status
    }

    pub(crate) const fn stage_status(self) -> StageRunStatus {
        self.stage_status
    }

    pub(crate) const fn delivery_status(self) -> crate::domain::DeliveryStatus {
        self.delivery_status
    }
}

struct Settlement {
    review_status: ValidatedReviewStatus,
    decision: Option<ValidatedReviewDecision>,
    comments: Option<String>,
    requested_changes: Option<Vec<String>>,
    reviewer_id: Option<String>,
    reviewed_at: Option<u64>,
}

/// Rebuilds the one current typed solution review from canonical Delivery facts.
pub(crate) fn resolve_current_solution_review(
    delivery: &Delivery,
) -> Result<Option<ValidatedSolutionReviewSet>, SolutionReviewError> {
    let snapshot = delivery.snapshot();
    let Some((attention, review_stage)) = current_plan_review(snapshot)? else {
        return Ok(None);
    };

    let context = decode_canonical_context(&attention.context)?;
    validate_context_encoding(&context)?;
    validate_context_payload(snapshot, &context)?;
    validate_current_authority(snapshot, attention, review_stage, &context)?;
    let settlement = resolve_settlement(snapshot, attention, review_stage, &context)?;

    Ok(Some(ValidatedSolutionReviewSet {
        delivery_id: context.delivery_id,
        delivery_spec_id: context.delivery_spec_id,
        delivery_spec_revision: context.delivery_spec_revision,
        planning_stage_run_id: context.planning_stage_run_id,
        planning_session_binding_id: context.planning_session_binding_id,
        review_stage_run_id: context.review_stage_run_id,
        attention_item_id: context.attention_item_id,
        review_set_sha256: context.review_set_sha256,
        solution_id: context.solution.id,
        summary: context.solution.summary,
        approach: context.solution.approach,
        components: context.solution.components,
        connections: context.solution.connections,
        architecture_diagram: context.architecture_diagram,
        process_diagram: context.process_diagram,
        risks: context.risks,
        unresolved_items: context.unresolved_items,
        task_proposals: context.task_proposals,
        prepared_at: context.prepared_at,
        review_status: settlement.review_status,
        decision: settlement.decision,
        comments: settlement.comments,
        requested_changes: settlement.requested_changes,
        reviewer_id: settlement.reviewer_id,
        reviewed_at: settlement.reviewed_at,
    }))
}

fn current_plan_review(
    snapshot: &crate::domain::DeliverySnapshot,
) -> Result<Option<(&AttentionItem, &StageRun)>, SolutionReviewError> {
    let Some(highest_attempt) = snapshot
        .stage_runs
        .iter()
        .filter(|run| run.stage == DeliveryStage::PlanReview)
        .map(|run| run.attempt)
        .max()
    else {
        return Ok(None);
    };
    let current_runs: Vec<_> = snapshot
        .stage_runs
        .iter()
        .filter(|run| run.stage == DeliveryStage::PlanReview && run.attempt == highest_attempt)
        .collect();
    let [review_stage] = current_runs.as_slice() else {
        return Err(review_error(
            SolutionReviewErrorCode::AmbiguousCurrentReview,
            "current Delivery has more than one plan-review StageRun at the highest attempt",
        ));
    };
    let current_attentions: Vec<_> = snapshot
        .attention_items
        .iter()
        .filter(|attention| {
            attention.delivery_id == snapshot.id
                && attention.delivery_spec_id == snapshot.spec.id
                && attention.stage_run_id.as_ref() == Some(&review_stage.id)
                && attention.item_type == AttentionItemType::DecisionRequired
                && attention.blocking
        })
        .collect();
    let [attention] = current_attentions.as_slice() else {
        return Err(if current_attentions.is_empty() {
            stale_authority("current plan-review StageRun has no exact Attention")
        } else {
            review_error(
                SolutionReviewErrorCode::AmbiguousCurrentReview,
                "current plan-review StageRun has more than one exact Attention",
            )
        });
    };
    Ok(Some((attention, review_stage)))
}

fn decode_canonical_context(raw: &str) -> Result<SolutionReviewContextV1, SolutionReviewError> {
    decode_canonical_json(
        raw,
        "solution-review context is not the one canonical v1 encoding",
    )
}

fn decode_canonical_decision(raw: &str) -> Result<SolutionReviewDecisionV1, SolutionReviewError> {
    decode_canonical_json(
        raw,
        "solution-review decision is not the one canonical v1 encoding",
    )
}

fn decode_canonical_json<T>(raw: &str, message: &str) -> Result<T, SolutionReviewError>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let value: T = serde_json::from_str(raw)
        .map_err(|_| review_error(SolutionReviewErrorCode::InvalidEncoding, message))?;
    let canonical = serde_json::to_string(&value).map_err(|_| {
        review_error(
            SolutionReviewErrorCode::InvalidEncoding,
            "solution-review canonical JSON encoding failed",
        )
    })?;
    if canonical != raw {
        return Err(review_error(
            SolutionReviewErrorCode::InvalidEncoding,
            message,
        ));
    }
    Ok(value)
}

fn validate_context_encoding(context: &SolutionReviewContextV1) -> Result<(), SolutionReviewError> {
    if context.schema_version != SOLUTION_REVIEW_SCHEMA_VERSION
        || context.protocol != SOLUTION_REVIEW_CONTEXT_PROTOCOL
        || !lowercase_sha256(&context.review_set_sha256)
    {
        return Err(review_error(
            SolutionReviewErrorCode::InvalidEncoding,
            "solution-review context version, protocol, or digest is invalid",
        ));
    }
    let expected = review_set_digest(context)?;
    if context.review_set_sha256 != expected {
        return Err(review_error(
            SolutionReviewErrorCode::InvalidEncoding,
            "solution-review context digest does not match its canonical content",
        ));
    }
    Ok(())
}

fn validate_context_payload(
    snapshot: &crate::domain::DeliverySnapshot,
    context: &SolutionReviewContextV1,
) -> Result<(), SolutionReviewError> {
    portable_id(&context.solution.id)?;
    safe_text(&context.solution.summary, MAX_TEXT_CODE_UNITS)?;
    safe_text_list(&context.solution.approach, true)?;
    safe_text_list(&context.risks, false)?;
    safe_text_list(&context.unresolved_items, false)?;
    validate_components(&context.solution.components)?;
    validate_connections(&context.solution.components, &context.solution.connections)?;
    validate_diagram(
        &context.architecture_diagram,
        ValidatedDiagramKind::SystemArchitecture,
    )?;
    validate_diagram(&context.process_diagram, ValidatedDiagramKind::ProcessFlow)?;
    validate_task_proposals(snapshot, &context.task_proposals)?;
    safe_time(context.prepared_at)?;
    Ok(())
}

fn validate_current_authority(
    snapshot: &crate::domain::DeliverySnapshot,
    attention: &AttentionItem,
    review_stage: &StageRun,
    context: &SolutionReviewContextV1,
) -> Result<(), SolutionReviewError> {
    if context.delivery_id != snapshot.id
        || context.delivery_spec_id != snapshot.spec.id
        || context.delivery_spec_revision != snapshot.spec.revision
        || context.review_stage_run_id != review_stage.id
        || context.attention_item_id != attention.id
        || attention.stage_run_id.as_ref() != Some(&review_stage.id)
        || attention.created_at_millis != context.prepared_at
    {
        return Err(stale_authority(
            "solution-review context does not match the current Delivery, Spec, review, or Attention",
        ));
    }

    let planning_stage = snapshot
        .stage_runs
        .iter()
        .find(|run| run.id == context.planning_stage_run_id)
        .ok_or_else(|| stale_authority("solution-review planning StageRun is missing"))?;
    let planning_bindings: Vec<_> = snapshot
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == planning_stage.id)
        .collect();
    let [planning_binding] = planning_bindings.as_slice() else {
        return Err(stale_authority(
            "solution-review planning StageRun requires one exact SessionBinding",
        ));
    };
    let review_has_binding = snapshot
        .session_bindings
        .iter()
        .any(|binding| binding.stage_run_id == review_stage.id);
    let planning_finished = planning_stage
        .finished_at_millis
        .ok_or_else(|| stale_authority("solution-review planning StageRun is not finished"))?;
    if planning_stage.delivery_task_id.is_some()
        || planning_stage.stage != DeliveryStage::Planning
        || planning_stage.actor_type != StageRunActorType::Codex
        || planning_stage.role != "planner"
        || planning_stage.status != StageRunStatus::Succeeded
        || planning_binding.id != context.planning_session_binding_id
        || planning_binding.delivery_task_id.is_some()
        || planning_binding.worker_session_id.is_none()
        || planning_binding.codex_thread_id.is_none()
        || review_stage.delivery_task_id.is_some()
        || review_stage.actor_type != StageRunActorType::Human
        || review_stage.role != "reviewer"
        || review_has_binding
        || context.prepared_at < planning_stage.started_at_millis
        || context.prepared_at < planning_binding.bound_at_millis
        || context.prepared_at > planning_finished
        || planning_finished > review_stage.started_at_millis
    {
        return Err(stale_authority(
            "solution-review planning binding or human review lifecycle is not current",
        ));
    }
    Ok(())
}

fn resolve_settlement(
    snapshot: &crate::domain::DeliverySnapshot,
    attention: &AttentionItem,
    review_stage: &StageRun,
    context: &SolutionReviewContextV1,
) -> Result<Settlement, SolutionReviewError> {
    if attention.status == AttentionItemStatus::Open {
        if attention.resolution.is_some()
            || attention.resolved_by.is_some()
            || attention.resolved_at_millis.is_some()
            || !matches!(
                review_stage.status,
                StageRunStatus::Waiting | StageRunStatus::Running
            )
            || review_stage.finished_at_millis.is_some()
            || snapshot.status != crate::domain::DeliveryStatus::NeedsAttention
        {
            return Err(stale_authority(
                "pending solution review has settlement facts or the wrong Delivery status",
            ));
        }
        return Ok(Settlement {
            review_status: ValidatedReviewStatus::Pending,
            decision: None,
            comments: None,
            requested_changes: None,
            reviewer_id: None,
            reviewed_at: None,
        });
    }

    let resolution = attention
        .resolution
        .as_deref()
        .ok_or_else(|| stale_authority("settled solution review has no decision"))?;
    let decision = decode_canonical_decision(resolution)?;
    validate_decision_encoding(&decision, context)?;
    let reviewer_id = attention
        .resolved_by
        .as_deref()
        .ok_or_else(|| stale_authority("settled solution review has no authenticated reviewer"))?;
    portable_id(reviewer_id)?;
    let reviewed_at = attention
        .resolved_at_millis
        .ok_or_else(|| stale_authority("settled solution review has no review time"))?;
    safe_time(reviewed_at)?;
    if attention
        .assigned_to
        .as_deref()
        .is_some_and(|assigned| assigned != reviewer_id)
        || review_stage.finished_at_millis != Some(reviewed_at)
        || review_stage.started_at_millis > reviewed_at
        || decision.delivery_id != snapshot.id
        || decision.delivery_spec_id != snapshot.spec.id
        || decision.delivery_spec_revision != snapshot.spec.revision
        || decision.review_stage_run_id != review_stage.id
        || decision.attention_item_id != attention.id
        || decision.review_set_sha256 != context.review_set_sha256
    {
        return Err(stale_authority(
            "solution-review decision does not match its current reviewer, time, or authority",
        ));
    }

    let (review_status, typed_decision, expected_attention, expected_stage, expected_delivery) =
        match decision.action {
            DecisionActionWire::Approve => (
                ValidatedReviewStatus::Approved,
                ValidatedReviewDecision::Approve,
                AttentionItemStatus::Resolved,
                StageRunStatus::Succeeded,
                crate::domain::DeliveryStatus::Executing,
            ),
            DecisionActionWire::RequestChanges => (
                ValidatedReviewStatus::ChangesRequested,
                ValidatedReviewDecision::RequestChanges,
                AttentionItemStatus::Dismissed,
                StageRunStatus::Failed,
                crate::domain::DeliveryStatus::Planning,
            ),
            DecisionActionWire::Reject => (
                ValidatedReviewStatus::Rejected,
                ValidatedReviewDecision::Reject,
                AttentionItemStatus::Dismissed,
                StageRunStatus::Failed,
                crate::domain::DeliveryStatus::Clarifying,
            ),
        };
    if attention.status != expected_attention
        || review_stage.status != expected_stage
        || snapshot.status != expected_delivery
    {
        return Err(stale_authority(
            "solution-review decision does not match Attention, StageRun, and Delivery settlement",
        ));
    }

    Ok(Settlement {
        review_status,
        decision: Some(typed_decision),
        comments: decision.comments,
        requested_changes: decision.requested_changes,
        reviewer_id: Some(reviewer_id.to_owned()),
        reviewed_at: Some(reviewed_at),
    })
}

/// Validates one pending plan-review decision before the Attention aggregate mutates.
pub(crate) fn validate_solution_review_settlement(
    delivery: &Delivery,
    attention_item_id: &AttentionItemId,
    review_stage_run_id: &StageRunId,
    actor: &str,
    resolution: &str,
    now_millis: u64,
) -> Result<ValidatedSolutionReviewSettlement, SolutionReviewError> {
    let review = resolve_current_solution_review(delivery)?
        .ok_or_else(|| stale_authority("plan-review settlement has no current review"))?;
    if review.review_status != ValidatedReviewStatus::Pending
        || &review.attention_item_id != attention_item_id
        || &review.review_stage_run_id != review_stage_run_id
    {
        return Err(stale_authority(
            "plan-review settlement is not for the exact pending review",
        ));
    }

    let snapshot = delivery.snapshot();
    let (attention, review_stage) = current_plan_review(snapshot)?
        .ok_or_else(|| stale_authority("plan-review settlement has no current authority"))?;
    portable_id(actor)?;
    safe_time(now_millis)?;
    if attention
        .assigned_to
        .as_deref()
        .is_some_and(|id| id != actor)
        || now_millis < attention.created_at_millis
        || now_millis < review_stage.started_at_millis
        || snapshot.attention_items.iter().any(|item| {
            item.id != attention.id && item.blocking && item.status == AttentionItemStatus::Open
        })
    {
        return Err(stale_authority(
            "plan-review settlement actor, time, or blocking authority is not current",
        ));
    }

    let context = decode_canonical_context(&attention.context)?;
    let decision = decode_canonical_decision(resolution)?;
    validate_decision_encoding(&decision, &context)?;
    let settlement = match decision.action {
        DecisionActionWire::Approve => ValidatedSolutionReviewSettlement {
            resolve_attention: true,
            attention_status: AttentionItemStatus::Resolved,
            stage_status: StageRunStatus::Succeeded,
            delivery_status: crate::domain::DeliveryStatus::Executing,
        },
        DecisionActionWire::RequestChanges => ValidatedSolutionReviewSettlement {
            resolve_attention: false,
            attention_status: AttentionItemStatus::Dismissed,
            stage_status: StageRunStatus::Failed,
            delivery_status: crate::domain::DeliveryStatus::Planning,
        },
        DecisionActionWire::Reject => ValidatedSolutionReviewSettlement {
            resolve_attention: false,
            attention_status: AttentionItemStatus::Dismissed,
            stage_status: StageRunStatus::Failed,
            delivery_status: crate::domain::DeliveryStatus::Clarifying,
        },
    };
    Ok(settlement)
}

fn validate_decision_encoding(
    decision: &SolutionReviewDecisionV1,
    context: &SolutionReviewContextV1,
) -> Result<(), SolutionReviewError> {
    if decision.schema_version != SOLUTION_REVIEW_SCHEMA_VERSION
        || decision.protocol != SOLUTION_REVIEW_DECISION_PROTOCOL
        || decision.delivery_id != context.delivery_id
        || decision.delivery_spec_id != context.delivery_spec_id
        || decision.delivery_spec_revision != context.delivery_spec_revision
        || decision.review_stage_run_id != context.review_stage_run_id
        || decision.attention_item_id != context.attention_item_id
        || decision.review_set_sha256 != context.review_set_sha256
    {
        return Err(stale_authority(
            "solution-review decision does not reference its exact context",
        ));
    }
    if let Some(comments) = &decision.comments {
        safe_text(comments, MAX_TEXT_CODE_UNITS)?;
    }
    if let Some(changes) = &decision.requested_changes {
        safe_text_list(changes, true)?;
    }
    let requested_changes_valid = match decision.action {
        DecisionActionWire::RequestChanges => decision
            .requested_changes
            .as_ref()
            .is_some_and(|changes| !changes.is_empty()),
        DecisionActionWire::Approve | DecisionActionWire::Reject => {
            decision.requested_changes.is_none()
        }
    };
    if !requested_changes_valid {
        return Err(review_error(
            SolutionReviewErrorCode::InvalidContent,
            "solution-review requestedChanges do not match the decision",
        ));
    }
    Ok(())
}

fn validate_components(
    components: &[ValidatedSolutionComponent],
) -> Result<(), SolutionReviewError> {
    bounded_collection(components.len(), true)?;
    let mut ids = HashSet::new();
    for component in components {
        portable_id(&component.id)?;
        safe_text(&component.label, MAX_TEXT_CODE_UNITS)?;
        safe_text(&component.responsibility, MAX_TEXT_CODE_UNITS)?;
        if let Some(boundary) = &component.trust_boundary {
            safe_text(boundary, MAX_TEXT_CODE_UNITS)?;
        }
        if PLATFORM_NODE_IDS.contains(&component.id.as_str()) || !ids.insert(&component.id) {
            return Err(invalid_content(
                "solution-review component identity is duplicated or reserved",
            ));
        }
        bounded_collection(component.repository_path_prefixes.len(), false)?;
        let mut prefixes = HashSet::new();
        for prefix in &component.repository_path_prefixes {
            repository_path_prefix(prefix)?;
            if !prefixes.insert(prefix) {
                return Err(invalid_content(
                    "solution-review component repeats a repository path prefix",
                ));
            }
        }
    }
    Ok(())
}

fn validate_connections(
    components: &[ValidatedSolutionComponent],
    connections: &[ValidatedSolutionConnection],
) -> Result<(), SolutionReviewError> {
    bounded_collection(connections.len(), false)?;
    let endpoints: HashSet<&str> = PLATFORM_NODE_IDS
        .iter()
        .copied()
        .chain(components.iter().map(|component| component.id.as_str()))
        .collect();
    let mut ids = HashSet::new();
    for connection in connections {
        portable_id(&connection.id)?;
        portable_id(&connection.from)?;
        portable_id(&connection.to)?;
        safe_text(&connection.label, MAX_TEXT_CODE_UNITS)?;
        if !ids.insert(&connection.id)
            || connection.from == connection.to
            || !endpoints.contains(connection.from.as_str())
            || !endpoints.contains(connection.to.as_str())
        {
            return Err(invalid_content(
                "solution-review connection identity or endpoints are invalid",
            ));
        }
    }
    Ok(())
}

fn validate_diagram(
    diagram: &ValidatedDiagram,
    expected_kind: ValidatedDiagramKind,
) -> Result<(), SolutionReviewError> {
    portable_id(&diagram.id)?;
    safe_text(&diagram.title, MAX_TEXT_CODE_UNITS)?;
    bounded_collection(diagram.nodes.len(), true)?;
    bounded_collection(diagram.edges.len(), false)?;
    if diagram.kind != expected_kind {
        return Err(invalid_content(
            "solution-review diagram kind does not match its field",
        ));
    }
    let mut node_ids = HashSet::new();
    for node in &diagram.nodes {
        portable_id(&node.id)?;
        safe_text(&node.label, MAX_TEXT_CODE_UNITS)?;
        safe_text(&node.description, MAX_TEXT_CODE_UNITS)?;
        if let Some(boundary) = &node.trust_boundary {
            safe_text(boundary, MAX_TEXT_CODE_UNITS)?;
        }
        if !node_ids.insert(&node.id) {
            return Err(invalid_content(
                "solution-review diagram repeats a node identity",
            ));
        }
    }
    let mut edge_ids = HashSet::new();
    for edge in &diagram.edges {
        portable_id(&edge.id)?;
        portable_id(&edge.from)?;
        portable_id(&edge.to)?;
        safe_text(&edge.label, MAX_TEXT_CODE_UNITS)?;
        if !edge_ids.insert(&edge.id)
            || edge.from == edge.to
            || !node_ids.contains(&edge.from)
            || !node_ids.contains(&edge.to)
        {
            return Err(invalid_content("solution-review diagram edge is invalid"));
        }
    }
    Ok(())
}

fn validate_task_proposals(
    snapshot: &crate::domain::DeliverySnapshot,
    task_proposals: &[DeliveryTaskProposal],
) -> Result<(), SolutionReviewError> {
    bounded_collection(task_proposals.len(), true)?;
    let current_criteria: HashSet<&str> = snapshot
        .spec
        .acceptance_criteria
        .iter()
        .map(|criterion| criterion.id.0.as_str())
        .collect();
    let mut task_ids = HashSet::new();
    let mut covered_criteria = HashSet::new();
    for proposal in task_proposals {
        portable_id(&proposal.id.0)?;
        safe_text(&proposal.title, MAX_TITLE_CODE_UNITS)?;
        safe_text(&proposal.goal, MAX_TEXT_CODE_UNITS)?;
        bounded_collection(proposal.acceptance_criterion_ids.len(), true)?;
        bounded_collection(proposal.blocked_by_task_ids.len(), false)?;
        if !task_ids.insert(proposal.id.0.as_str()) {
            return Err(invalid_content(
                "solution-review task proposal identity is duplicated",
            ));
        }
        let mut proposal_criteria = HashSet::new();
        for criterion in &proposal.acceptance_criterion_ids {
            portable_id(&criterion.0)?;
            if !current_criteria.contains(criterion.0.as_str())
                || !proposal_criteria.insert(criterion.0.as_str())
            {
                return Err(invalid_content(
                    "solution-review task proposal has a duplicate or foreign criterion",
                ));
            }
            covered_criteria.insert(criterion.0.as_str());
        }
        let mut dependencies = HashSet::new();
        for dependency in &proposal.blocked_by_task_ids {
            portable_id(&dependency.0)?;
            if dependency == &proposal.id || !dependencies.insert(dependency.0.as_str()) {
                return Err(invalid_content(
                    "solution-review task proposal dependency is self-referential or duplicated",
                ));
            }
        }
    }
    if covered_criteria != current_criteria {
        return Err(invalid_content(
            "solution-review task proposals do not cover every current criterion",
        ));
    }
    let proposals_by_id: HashMap<&str, &DeliveryTaskProposal> = task_proposals
        .iter()
        .map(|proposal| (proposal.id.0.as_str(), proposal))
        .collect();
    for proposal in task_proposals {
        if proposal
            .blocked_by_task_ids
            .iter()
            .any(|dependency| !proposals_by_id.contains_key(dependency.0.as_str()))
        {
            return Err(invalid_content(
                "solution-review task proposal dependency is missing",
            ));
        }
    }
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for proposal in task_proposals {
        visit_task_proposal(proposal, &proposals_by_id, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit_task_proposal<'proposal>(
    proposal: &'proposal DeliveryTaskProposal,
    proposals: &HashMap<&'proposal str, &'proposal DeliveryTaskProposal>,
    visiting: &mut HashSet<&'proposal str>,
    visited: &mut HashSet<&'proposal str>,
) -> Result<(), SolutionReviewError> {
    let id = proposal.id.0.as_str();
    if visiting.contains(id) {
        return Err(invalid_content(
            "solution-review task proposal dependencies contain a cycle",
        ));
    }
    if visited.contains(id) {
        return Ok(());
    }
    visiting.insert(id);
    for dependency in &proposal.blocked_by_task_ids {
        visit_task_proposal(
            proposals[dependency.0.as_str()],
            proposals,
            visiting,
            visited,
        )?;
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

fn review_set_digest(context: &SolutionReviewContextV1) -> Result<String, SolutionReviewError> {
    let input = ReviewDigestInput {
        schema_version: context.schema_version,
        protocol: &context.protocol,
        delivery_id: &context.delivery_id,
        delivery_spec_id: &context.delivery_spec_id,
        delivery_spec_revision: context.delivery_spec_revision,
        planning_stage_run_id: &context.planning_stage_run_id,
        planning_session_binding_id: &context.planning_session_binding_id,
        review_stage_run_id: &context.review_stage_run_id,
        attention_item_id: &context.attention_item_id,
        solution: &context.solution,
        architecture_diagram: &context.architecture_diagram,
        process_diagram: &context.process_diagram,
        risks: &context.risks,
        unresolved_items: &context.unresolved_items,
        task_proposals: &context.task_proposals,
        prepared_at: context.prepared_at,
    };
    let encoded = serde_json::to_vec(&input).map_err(|_| {
        review_error(
            SolutionReviewErrorCode::InvalidEncoding,
            "solution-review digest input cannot be encoded",
        )
    })?;
    let digest = Sha256::digest(encoded);
    let mut encoded_digest = String::with_capacity(64);
    for byte in digest {
        encoded_digest.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        encoded_digest.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    Ok(encoded_digest)
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn portable_id(value: &str) -> Result<(), SolutionReviewError> {
    let mut bytes = value.bytes();
    let valid = value.len() <= 200
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(invalid_content(
            "solution-review identity is not bounded and portable",
        ))
    }
}

fn safe_text(value: &str, maximum: usize) -> Result<(), SolutionReviewError> {
    let forbidden_control = value
        .chars()
        .any(|character| matches!(u32::from(character), 0..=8 | 11..=12 | 14..=31 | 127));
    if value.trim().is_empty() || value.encode_utf16().count() > maximum || forbidden_control {
        Err(invalid_content(
            "solution-review text is empty, oversized, or contains a control character",
        ))
    } else {
        Ok(())
    }
}

fn safe_text_list(values: &[String], required: bool) -> Result<(), SolutionReviewError> {
    bounded_collection(values.len(), required)?;
    let mut unique = HashSet::new();
    for value in values {
        safe_text(value, MAX_TEXT_CODE_UNITS)?;
        if !unique.insert(value) {
            return Err(invalid_content(
                "solution-review text collection contains duplicates",
            ));
        }
    }
    Ok(())
}

fn bounded_collection(length: usize, required: bool) -> Result<(), SolutionReviewError> {
    if length > MAX_COLLECTION_ITEMS || (required && length == 0) {
        Err(invalid_content(
            "solution-review collection is empty or exceeds its bound",
        ))
    } else {
        Ok(())
    }
}

fn repository_path_prefix(value: &str) -> Result<(), SolutionReviewError> {
    let valid = !value.is_empty()
        && value.len() <= MAX_REPOSITORY_PATH_LENGTH
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains('\\')
        && !value.bytes().any(|byte| byte <= 31 || byte == 127)
        && !value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        && !value.contains(['*', '?', '[', ']', '{', '}', '!']);
    if valid {
        Ok(())
    } else {
        Err(invalid_content(
            "solution-review repository path prefix is unsafe",
        ))
    }
}

fn safe_time(value: u64) -> Result<(), SolutionReviewError> {
    if value <= MAX_SAFE_INTEGER {
        Ok(())
    } else {
        Err(invalid_content(
            "solution-review time exceeds the safe integer range",
        ))
    }
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn invalid_content(message: &str) -> SolutionReviewError {
    review_error(SolutionReviewErrorCode::InvalidContent, message)
}

fn stale_authority(message: &str) -> SolutionReviewError {
    review_error(SolutionReviewErrorCode::StaleAuthority, message)
}

fn review_error(code: SolutionReviewErrorCode, message: &str) -> SolutionReviewError {
    SolutionReviewError {
        code,
        message: message.to_owned(),
    }
}

/// High-level semantic fixtures for exercising the real solution-review
/// transitions from integration tests. The canonical context, decision,
/// digest, and validated authority facts remain private to this module.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    use super::{
        AcceptanceCriterionId, DecisionActionWire, Delivery, DeliveryStage, DeliveryTaskId,
        DeliveryTaskProposal, Error, SOLUTION_REVIEW_CONTEXT_PROTOCOL,
        SOLUTION_REVIEW_DECISION_PROTOCOL, SOLUTION_REVIEW_SCHEMA_VERSION, SessionBindingId,
        SolutionReviewContextV1, SolutionReviewDecisionV1, SolutionWire, StageRunActorType,
        StageRunId, ValidatedDiagram, ValidatedDiagramEdge, ValidatedDiagramKind,
        ValidatedDiagramNode, ValidatedDiagramNodeKind, ValidatedReviewStatus,
        ValidatedSolutionComponent, ValidatedSolutionComponentKind, ValidatedSolutionConnection,
        fmt, resolve_current_solution_review, review_set_digest,
    };
    use crate::application::{
        attention::{
            AttentionDecision, ResolveAttentionInput, ResolvedAttentionTransition,
            resolve_attention,
        },
        stage::{
            AdvanceStageInput, ReviewAttentionSeed, StageAdvanceEffect, StageAdvanceResult, advance,
        },
    };
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum SolutionComponentKindFixture {
        Component,
        External,
        DataStore,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct SolutionComponentFixture {
        pub id: String,
        pub label: String,
        pub responsibility: String,
        pub kind: SolutionComponentKindFixture,
        pub trust_boundary: Option<String>,
        pub unresolved: bool,
        pub repository_path_prefixes: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct SolutionConnectionFixture {
        pub id: String,
        pub from: String,
        pub to: String,
        pub label: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct SolutionFixture {
        pub id: String,
        pub summary: String,
        pub approach: Vec<String>,
        pub components: Vec<SolutionComponentFixture>,
        pub connections: Vec<SolutionConnectionFixture>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum SolutionDiagramKindFixture {
        SystemArchitecture,
        ProcessFlow,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum SolutionDiagramNodeKindFixture {
        Interaction,
        DeliveryControl,
        Execution,
        Repository,
        Component,
        External,
        DataStore,
        Stage,
        Decision,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct SolutionDiagramNodeFixture {
        pub id: String,
        pub label: String,
        pub description: String,
        pub kind: SolutionDiagramNodeKindFixture,
        pub trust_boundary: Option<String>,
        pub unresolved: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct SolutionDiagramEdgeFixture {
        pub id: String,
        pub from: String,
        pub to: String,
        pub label: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct SolutionDiagramFixture {
        pub id: String,
        pub kind: SolutionDiagramKindFixture,
        pub title: String,
        pub nodes: Vec<SolutionDiagramNodeFixture>,
        pub edges: Vec<SolutionDiagramEdgeFixture>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct SolutionReviewTaskProposalFixture {
        pub id: DeliveryTaskId,
        pub title: String,
        pub goal: String,
        pub acceptance_criterion_ids: Vec<AcceptanceCriterionId>,
        pub blocked_by_task_ids: Vec<DeliveryTaskId>,
    }

    /// Semantic review content. It intentionally contains no Delivery,
    /// `StageRun`, `SessionBinding`, Attention, protocol, or digest authority.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SolutionReviewFixture {
        pub attention_title: String,
        pub assigned_to: String,
        pub solution: SolutionFixture,
        pub architecture_diagram: SolutionDiagramFixture,
        pub process_diagram: SolutionDiagramFixture,
        pub risks: Vec<String>,
        pub unresolved_items: Vec<String>,
        pub task_proposals: Vec<SolutionReviewTaskProposalFixture>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum InvalidTaskProposalFixture {
        DependencyCycle,
        MissingDependency,
        DuplicateTaskId,
        DuplicateCriterionId,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum SolutionReviewDecisionFixture {
        Approve {
            comments: Option<String>,
        },
        RequestChanges {
            comments: Option<String>,
            requested_changes: Vec<String>,
        },
        Reject {
            comments: Option<String>,
        },
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SolutionReviewFixtureError {
        message: String,
    }

    impl SolutionReviewFixtureError {
        pub fn message(&self) -> &str {
            &self.message
        }
    }

    impl fmt::Display for SolutionReviewFixtureError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.message)
        }
    }

    impl Error for SolutionReviewFixtureError {}

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct PreparedSolutionReviewFixture {
        transition: StageAdvanceResult,
        review_set_sha256: String,
    }

    impl PreparedSolutionReviewFixture {
        pub fn transition(&self) -> &StageAdvanceResult {
            &self.transition
        }

        pub fn into_transition(self) -> StageAdvanceResult {
            self.transition
        }

        pub fn review_set_sha256(&self) -> &str {
            &self.review_set_sha256
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SettledSolutionReviewFixture {
        transition: ResolvedAttentionTransition,
        review_set_sha256: String,
    }

    impl SettledSolutionReviewFixture {
        pub fn transition(&self) -> &ResolvedAttentionTransition {
            &self.transition
        }

        pub fn into_transition(self) -> ResolvedAttentionTransition {
            self.transition
        }

        pub fn review_set_sha256(&self) -> &str {
            &self.review_set_sha256
        }
    }

    /// Builds and validates the exact current plan-review context, then calls
    /// the production stage coordinator. Callers never encode authority JSON
    /// or calculate a review-set digest.
    pub fn prepare_solution_review_fixture(
        delivery: &Delivery,
        mut input: AdvanceStageInput,
        fixture: SolutionReviewFixture,
    ) -> Result<PreparedSolutionReviewFixture, SolutionReviewFixtureError> {
        if input.review.is_some() {
            return Err(fixture_error(
                "solution-review fixture owns the canonical review Attention seed",
            ));
        }
        let (planning_stage_run_id, planning_session_binding_id) =
            current_planning_authority(delivery)?;
        let review_stage_run_id = input.identities.stage_run_id.clone();
        let attention_item_id = input.identities.attention_item_id.clone();
        let prepared_at = input.now_millis;
        let task_proposals = normalized_task_proposals(delivery, fixture.task_proposals)
            .into_iter()
            .map(DeliveryTaskProposal::from)
            .collect();
        let mut context = SolutionReviewContextV1 {
            schema_version: SOLUTION_REVIEW_SCHEMA_VERSION,
            protocol: SOLUTION_REVIEW_CONTEXT_PROTOCOL.to_owned(),
            delivery_id: delivery.id().clone(),
            delivery_spec_id: delivery.snapshot().spec.id.clone(),
            delivery_spec_revision: delivery.snapshot().spec.revision,
            planning_stage_run_id,
            planning_session_binding_id,
            review_stage_run_id,
            attention_item_id,
            solution: fixture.solution.into(),
            architecture_diagram: fixture.architecture_diagram.into(),
            process_diagram: fixture.process_diagram.into(),
            risks: fixture.risks,
            unresolved_items: fixture.unresolved_items,
            task_proposals,
            prepared_at,
            review_set_sha256: String::new(),
        };
        context.review_set_sha256 =
            review_set_digest(&context).map_err(|error| fixture_error(error.to_string()))?;
        let review_set_sha256 = context.review_set_sha256.clone();
        let encoded = serde_json::to_string(&context).map_err(|error| {
            fixture_error(format!(
                "canonical solution-review context encoding failed: {error}"
            ))
        })?;
        input.review = Some(ReviewAttentionSeed {
            title: fixture.attention_title,
            context: encoded,
            assigned_to: fixture.assigned_to,
        });
        let transition =
            advance(delivery, input).map_err(|error| fixture_error(error.to_string()))?;
        if !matches!(transition.effect, StageAdvanceEffect::Review(_)) {
            return Err(fixture_error(
                "solution-review fixture did not advance to a human review",
            ));
        }
        let resolved = resolve_current_solution_review(&transition.delivery)
            .map_err(|error| fixture_error(error.to_string()))?
            .ok_or_else(|| fixture_error("prepared Delivery has no current solution review"))?;
        if resolved.review_status != ValidatedReviewStatus::Pending
            || resolved.review_set_sha256 != review_set_sha256
        {
            return Err(fixture_error(
                "prepared solution review did not resolve to the exact pending review set",
            ));
        }
        Ok(PreparedSolutionReviewFixture {
            transition,
            review_set_sha256,
        })
    }

    /// Reconstructs an exact typed decision for the current pending review and
    /// delegates all state movement to the production Attention transition.
    pub fn settle_solution_review_fixture(
        delivery: &Delivery,
        actor: &str,
        now_millis: u64,
        decision: SolutionReviewDecisionFixture,
    ) -> Result<SettledSolutionReviewFixture, SolutionReviewFixtureError> {
        let current = resolve_current_solution_review(delivery)
            .map_err(|error| fixture_error(error.to_string()))?
            .ok_or_else(|| fixture_error("Delivery has no current solution review"))?;
        if current.review_status != ValidatedReviewStatus::Pending {
            return Err(fixture_error("solution review is not pending"));
        }
        let (action, comments, requested_changes, attention_decision, expected_status) =
            match decision {
                SolutionReviewDecisionFixture::Approve { comments } => (
                    DecisionActionWire::Approve,
                    comments,
                    None,
                    AttentionDecision::Resolved,
                    ValidatedReviewStatus::Approved,
                ),
                SolutionReviewDecisionFixture::RequestChanges {
                    comments,
                    requested_changes,
                } => (
                    DecisionActionWire::RequestChanges,
                    comments,
                    Some(requested_changes),
                    AttentionDecision::Dismissed,
                    ValidatedReviewStatus::ChangesRequested,
                ),
                SolutionReviewDecisionFixture::Reject { comments } => (
                    DecisionActionWire::Reject,
                    comments,
                    None,
                    AttentionDecision::Dismissed,
                    ValidatedReviewStatus::Rejected,
                ),
            };
        let review_set_sha256 = current.review_set_sha256.clone();
        let attention_item_id = current.attention_item_id.clone();
        let review_stage_run_id = current.review_stage_run_id.clone();
        let resolution = SolutionReviewDecisionV1 {
            schema_version: SOLUTION_REVIEW_SCHEMA_VERSION,
            protocol: SOLUTION_REVIEW_DECISION_PROTOCOL.to_owned(),
            delivery_id: current.delivery_id.clone(),
            delivery_spec_id: current.delivery_spec_id.clone(),
            delivery_spec_revision: current.delivery_spec_revision,
            review_stage_run_id: review_stage_run_id.clone(),
            attention_item_id: attention_item_id.clone(),
            review_set_sha256: review_set_sha256.clone(),
            action,
            comments,
            requested_changes,
        };
        let resolution = serde_json::to_string(&resolution).map_err(|error| {
            fixture_error(format!(
                "canonical solution-review decision encoding failed: {error}"
            ))
        })?;
        let attention = delivery
            .snapshot()
            .attention_items
            .iter()
            .find(|item| item.id == attention_item_id)
            .ok_or_else(|| fixture_error("current solution-review Attention disappeared"))?;
        let transition = resolve_attention(
            delivery,
            ResolveAttentionInput {
                expected_revision: delivery.revision(),
                attention_item_id,
                stage_run_id: review_stage_run_id,
                expected_context: attention.context.clone(),
                actor: actor.to_owned(),
                decision: attention_decision,
                resolution,
                now_millis,
            },
        )
        .map_err(|error| fixture_error(error.to_string()))?;
        let settled = resolve_current_solution_review(transition.delivery())
            .map_err(|error| fixture_error(error.to_string()))?
            .ok_or_else(|| fixture_error("settled Delivery lost its solution review"))?;
        if settled.review_status != expected_status
            || settled.review_set_sha256 != review_set_sha256
            || settled.reviewer_id.as_deref() != Some(actor)
            || settled.reviewed_at != Some(now_millis)
        {
            return Err(fixture_error(
                "solution-review settlement did not preserve its exact authority",
            ));
        }
        Ok(SettledSolutionReviewFixture {
            transition,
            review_set_sha256,
        })
    }

    /// Produces semantic invalid graphs without exposing canonical context or
    /// digest construction. Passing one of these graphs to `prepare` must be
    /// rejected by the production solution-review resolver.
    pub fn invalid_task_proposals_fixture(
        delivery: &Delivery,
        invalid: InvalidTaskProposalFixture,
    ) -> Vec<SolutionReviewTaskProposalFixture> {
        let mut first = default_task_proposal(delivery);
        match invalid {
            InvalidTaskProposalFixture::MissingDependency => {
                first.blocked_by_task_ids = vec![DeliveryTaskId("task:missing".to_owned())];
                vec![first]
            }
            InvalidTaskProposalFixture::DuplicateTaskId => vec![first.clone(), first],
            InvalidTaskProposalFixture::DuplicateCriterionId => {
                if let Some(criterion) = first.acceptance_criterion_ids.first().cloned() {
                    first.acceptance_criterion_ids.push(criterion);
                }
                vec![first]
            }
            InvalidTaskProposalFixture::DependencyCycle => {
                let second_id = DeliveryTaskId("task:fixture:cycle".to_owned());
                let first_id = first.id.clone();
                first.blocked_by_task_ids = vec![second_id.clone()];
                let second = SolutionReviewTaskProposalFixture {
                    id: second_id,
                    title: "Cycle dependency".to_owned(),
                    goal: "Exercise canonical cycle rejection.".to_owned(),
                    acceptance_criterion_ids: first.acceptance_criterion_ids.clone(),
                    blocked_by_task_ids: vec![first_id],
                };
                vec![first, second]
            }
        }
    }

    fn current_planning_authority(
        delivery: &Delivery,
    ) -> Result<(StageRunId, SessionBindingId), SolutionReviewFixtureError> {
        let highest_attempt = delivery
            .snapshot()
            .stage_runs
            .iter()
            .filter(|run| {
                run.delivery_task_id.is_none()
                    && run.stage == DeliveryStage::Planning
                    && run.actor_type == StageRunActorType::Codex
                    && run.role == "planner"
            })
            .map(|run| run.attempt)
            .max()
            .ok_or_else(|| fixture_error("Delivery has no planning StageRun"))?;
        let planning_runs: Vec<_> = delivery
            .snapshot()
            .stage_runs
            .iter()
            .filter(|run| {
                run.delivery_task_id.is_none()
                    && run.stage == DeliveryStage::Planning
                    && run.actor_type == StageRunActorType::Codex
                    && run.role == "planner"
                    && run.attempt == highest_attempt
            })
            .collect();
        let [planning] = planning_runs.as_slice() else {
            return Err(fixture_error(
                "Delivery does not have one exact current planning StageRun",
            ));
        };
        let bindings: Vec<_> = delivery
            .snapshot()
            .session_bindings
            .iter()
            .filter(|binding| binding.stage_run_id == planning.id)
            .collect();
        let [binding] = bindings.as_slice() else {
            return Err(fixture_error(
                "current planning StageRun does not have one exact SessionBinding",
            ));
        };
        Ok((planning.id.clone(), binding.id.clone()))
    }

    fn normalized_task_proposals(
        delivery: &Delivery,
        proposals: Vec<SolutionReviewTaskProposalFixture>,
    ) -> Vec<SolutionReviewTaskProposalFixture> {
        if proposals.is_empty() {
            vec![default_task_proposal(delivery)]
        } else {
            proposals
        }
    }

    fn default_task_proposal(delivery: &Delivery) -> SolutionReviewTaskProposalFixture {
        SolutionReviewTaskProposalFixture {
            id: DeliveryTaskId("task:fixture".to_owned()),
            title: "Implement the approved Delivery solution".to_owned(),
            goal: "Satisfy every acceptance criterion in the current DeliverySpec.".to_owned(),
            acceptance_criterion_ids: delivery
                .snapshot()
                .spec
                .acceptance_criteria
                .iter()
                .map(|criterion| criterion.id.clone())
                .collect(),
            blocked_by_task_ids: Vec::new(),
        }
    }

    fn fixture_error(message: impl Into<String>) -> SolutionReviewFixtureError {
        SolutionReviewFixtureError {
            message: message.into(),
        }
    }

    impl From<SolutionReviewTaskProposalFixture> for DeliveryTaskProposal {
        fn from(value: SolutionReviewTaskProposalFixture) -> Self {
            Self {
                id: value.id,
                title: value.title,
                goal: value.goal,
                acceptance_criterion_ids: value.acceptance_criterion_ids,
                blocked_by_task_ids: value.blocked_by_task_ids,
            }
        }
    }

    impl From<SolutionFixture> for SolutionWire {
        fn from(value: SolutionFixture) -> Self {
            Self {
                id: value.id,
                summary: value.summary,
                approach: value.approach,
                components: value.components.into_iter().map(Into::into).collect(),
                connections: value.connections.into_iter().map(Into::into).collect(),
            }
        }
    }

    impl From<SolutionComponentFixture> for ValidatedSolutionComponent {
        fn from(value: SolutionComponentFixture) -> Self {
            Self {
                id: value.id,
                label: value.label,
                responsibility: value.responsibility,
                kind: match value.kind {
                    SolutionComponentKindFixture::Component => {
                        ValidatedSolutionComponentKind::Component
                    }
                    SolutionComponentKindFixture::External => {
                        ValidatedSolutionComponentKind::External
                    }
                    SolutionComponentKindFixture::DataStore => {
                        ValidatedSolutionComponentKind::DataStore
                    }
                },
                trust_boundary: value.trust_boundary,
                unresolved: value.unresolved,
                repository_path_prefixes: value.repository_path_prefixes,
            }
        }
    }

    impl From<SolutionConnectionFixture> for ValidatedSolutionConnection {
        fn from(value: SolutionConnectionFixture) -> Self {
            Self {
                id: value.id,
                from: value.from,
                to: value.to,
                label: value.label,
            }
        }
    }

    impl From<SolutionDiagramFixture> for ValidatedDiagram {
        fn from(value: SolutionDiagramFixture) -> Self {
            Self {
                id: value.id,
                kind: match value.kind {
                    SolutionDiagramKindFixture::SystemArchitecture => {
                        ValidatedDiagramKind::SystemArchitecture
                    }
                    SolutionDiagramKindFixture::ProcessFlow => ValidatedDiagramKind::ProcessFlow,
                },
                title: value.title,
                nodes: value.nodes.into_iter().map(Into::into).collect(),
                edges: value.edges.into_iter().map(Into::into).collect(),
            }
        }
    }

    impl From<SolutionDiagramNodeFixture> for ValidatedDiagramNode {
        fn from(value: SolutionDiagramNodeFixture) -> Self {
            Self {
                id: value.id,
                label: value.label,
                description: value.description,
                kind: match value.kind {
                    SolutionDiagramNodeKindFixture::Interaction => {
                        ValidatedDiagramNodeKind::Interaction
                    }
                    SolutionDiagramNodeKindFixture::DeliveryControl => {
                        ValidatedDiagramNodeKind::DeliveryControl
                    }
                    SolutionDiagramNodeKindFixture::Execution => {
                        ValidatedDiagramNodeKind::Execution
                    }
                    SolutionDiagramNodeKindFixture::Repository => {
                        ValidatedDiagramNodeKind::Repository
                    }
                    SolutionDiagramNodeKindFixture::Component => {
                        ValidatedDiagramNodeKind::Component
                    }
                    SolutionDiagramNodeKindFixture::External => ValidatedDiagramNodeKind::External,
                    SolutionDiagramNodeKindFixture::DataStore => {
                        ValidatedDiagramNodeKind::DataStore
                    }
                    SolutionDiagramNodeKindFixture::Stage => ValidatedDiagramNodeKind::Stage,
                    SolutionDiagramNodeKindFixture::Decision => ValidatedDiagramNodeKind::Decision,
                },
                trust_boundary: value.trust_boundary,
                unresolved: value.unresolved,
            }
        }
    }

    impl From<SolutionDiagramEdgeFixture> for ValidatedDiagramEdge {
        fn from(value: SolutionDiagramEdgeFixture) -> Self {
            Self {
                id: value.id,
                from: value.from,
                to: value.to,
                label: value.label,
            }
        }
    }
}

#[cfg(test)]
pub(crate) use tests::{
    ReviewFixtureState, duplicate_task_and_criterion_fixtures, empty_task_proposals_fixture,
    invalid_criterion_fixtures, invalid_dependency_fixtures, ordered_task_proposals_fixture,
    review_delivery, with_newer_review_attempt,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};
    use winwincode_domain::{
        CodexThreadId, ExecutionAckSequence, ExecutionJobId, FencingToken, LeaseId,
        ProductSessionId, RequestId, WorkerId, WorkerInstanceId, WorkerSessionId,
    };

    use super::*;
    use crate::application::attention::{
        AttentionDecision, ResolveAttentionInput, resolve_attention,
    };
    use crate::{
        domain::{
            AttentionItem, DeliverySnapshot, DeliveryStatus, SessionBinding, StageRun, test_fixture,
        },
        projection::{
            ProjectionInput, SolutionReviewDecisionProjection, SolutionReviewStatusProjection,
            project_delivery_detail,
        },
        store::{
            AppendDelivery, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
            DeliveryMutationOperation, DeliveryStore, DeliveryStoreErrorCode,
            InMemoryDeliveryJournal, ResolveDeliveryAttention,
        },
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ReviewFixtureState {
        Pending,
        Approved,
        ChangesRequested,
        Rejected,
    }

    fn diagram(id: &str, kind: ValidatedDiagramKind) -> ValidatedDiagram {
        ValidatedDiagram {
            id: id.into(),
            kind,
            title: format!("{id} title"),
            nodes: vec![
                ValidatedDiagramNode {
                    id: "node:control-plane".into(),
                    label: "Control Plane".into(),
                    description: "Owns delivery decisions.".into(),
                    kind: ValidatedDiagramNodeKind::DeliveryControl,
                    trust_boundary: Some("control-plane".into()),
                    unresolved: false,
                },
                ValidatedDiagramNode {
                    id: "node:worker".into(),
                    label: "Worker".into(),
                    description: "Executes approved work.".into(),
                    kind: ValidatedDiagramNodeKind::Execution,
                    trust_boundary: Some("worker".into()),
                    unresolved: false,
                },
            ],
            edges: vec![ValidatedDiagramEdge {
                id: "edge:dispatch".into(),
                from: "node:control-plane".into(),
                to: "node:worker".into(),
                label: "dispatches".into(),
            }],
        }
    }

    fn context_for_attempt(
        snapshot: &DeliverySnapshot,
        planning_stage_run_id: StageRunId,
        planning_session_binding_id: SessionBindingId,
        review_stage_run_id: StageRunId,
        attention_item_id: AttentionItemId,
        prepared_at: u64,
    ) -> SolutionReviewContextV1 {
        let task_proposals = vec![DeliveryTaskProposal {
            id: DeliveryTaskId("task:invitation".into()),
            title: "Implement invitation flow".into(),
            goal: "Deliver every current acceptance criterion.".into(),
            acceptance_criterion_ids: snapshot
                .spec
                .acceptance_criteria
                .iter()
                .map(|criterion| criterion.id.clone())
                .collect(),
            blocked_by_task_ids: vec![],
        }];
        let mut context = SolutionReviewContextV1 {
            schema_version: SOLUTION_REVIEW_SCHEMA_VERSION,
            protocol: SOLUTION_REVIEW_CONTEXT_PROTOCOL.into(),
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            delivery_spec_revision: snapshot.spec.revision,
            planning_stage_run_id,
            planning_session_binding_id,
            review_stage_run_id,
            attention_item_id,
            solution: SolutionWire {
                id: "solution:invitation".into(),
                summary: "Implement one controlled invitation flow.".into(),
                approach: vec!["Define the API.".into(), "Verify every criterion.".into()],
                components: vec![
                    ValidatedSolutionComponent {
                        id: "component:web".into(),
                        label: "Web".into(),
                        responsibility: "Renders invitation state.".into(),
                        kind: ValidatedSolutionComponentKind::Component,
                        trust_boundary: Some("browser".into()),
                        unresolved: false,
                        repository_path_prefixes: vec!["apps/web".into()],
                    },
                    ValidatedSolutionComponent {
                        id: "component:api".into(),
                        label: "API".into(),
                        responsibility: "Accepts invitations exactly once.".into(),
                        kind: ValidatedSolutionComponentKind::Component,
                        trust_boundary: Some("control-plane".into()),
                        unresolved: false,
                        repository_path_prefixes: vec!["crates/api".into()],
                    },
                ],
                connections: vec![ValidatedSolutionConnection {
                    id: "connection:web-api".into(),
                    from: "component:web".into(),
                    to: "component:api".into(),
                    label: "HTTP".into(),
                }],
            },
            architecture_diagram: diagram(
                "diagram:architecture",
                ValidatedDiagramKind::SystemArchitecture,
            ),
            process_diagram: diagram("diagram:process", ValidatedDiagramKind::ProcessFlow),
            risks: vec!["Invitation replay.".into()],
            unresolved_items: vec!["Confirm expiration policy.".into()],
            task_proposals,
            prepared_at,
            review_set_sha256: String::new(),
        };
        context.review_set_sha256 = review_set_digest(&context).expect("context digest");
        context
    }

    fn context_for(snapshot: &DeliverySnapshot) -> SolutionReviewContextV1 {
        context_for_attempt(
            snapshot,
            StageRunId("stage:planning".into()),
            SessionBindingId("binding:planning".into()),
            StageRunId("stage:plan-review".into()),
            AttentionItemId("attention:plan-review".into()),
            1_800_000_000_019,
        )
    }

    fn decision_for(
        context: &SolutionReviewContextV1,
        state: ReviewFixtureState,
    ) -> SolutionReviewDecisionV1 {
        let (action, comments, requested_changes) = match state {
            ReviewFixtureState::Approved => (
                DecisionActionWire::Approve,
                Some("Approved after review.".into()),
                None,
            ),
            ReviewFixtureState::ChangesRequested => (
                DecisionActionWire::RequestChanges,
                Some("Please address the requested change.".into()),
                Some(vec!["Add the replay boundary test.".into()]),
            ),
            ReviewFixtureState::Rejected => (DecisionActionWire::Reject, None, None),
            ReviewFixtureState::Pending => panic!("pending review has no decision"),
        };
        SolutionReviewDecisionV1 {
            schema_version: SOLUTION_REVIEW_SCHEMA_VERSION,
            protocol: SOLUTION_REVIEW_DECISION_PROTOCOL.into(),
            delivery_id: context.delivery_id.clone(),
            delivery_spec_id: context.delivery_spec_id.clone(),
            delivery_spec_revision: context.delivery_spec_revision,
            review_stage_run_id: context.review_stage_run_id.clone(),
            attention_item_id: context.attention_item_id.clone(),
            review_set_sha256: context.review_set_sha256.clone(),
            action,
            comments,
            requested_changes,
        }
    }

    pub(crate) fn review_delivery(state: ReviewFixtureState) -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = match state {
            ReviewFixtureState::Pending => DeliveryStatus::NeedsAttention,
            ReviewFixtureState::Approved => DeliveryStatus::Executing,
            ReviewFixtureState::ChangesRequested => DeliveryStatus::Planning,
            ReviewFixtureState::Rejected => DeliveryStatus::Clarifying,
        };
        snapshot.tasks.clear();
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.attention_items.clear();

        let planning = &mut snapshot.stage_runs[0];
        planning.id = StageRunId("stage:planning".into());
        planning.delivery_task_id = None;
        planning.stage = DeliveryStage::Planning;
        planning.actor_type = StageRunActorType::Codex;
        planning.role = "planner".into();
        planning.status = StageRunStatus::Succeeded;
        planning.attempt = 1;
        planning.started_at_millis = 1_800_000_000_010;
        planning.finished_at_millis = Some(1_800_000_000_020);

        let binding = &mut snapshot.session_bindings[0];
        binding.id = SessionBindingId("binding:planning".into());
        binding.delivery_task_id = None;
        binding.stage_run_id = planning.id.clone();
        binding.product_session_id = ProductSessionId("product:planning".into());
        binding.execution_job_id = ExecutionJobId("job:planning".into());
        binding.worker_session_id = Some(WorkerSessionId("worker-session:planning".into()));
        binding.codex_thread_id = Some(CodexThreadId("codex-thread:planning".into()));
        binding.bound_at_millis = 1_800_000_000_011;

        let (review_status, finished_at) = match state {
            ReviewFixtureState::Pending => (StageRunStatus::Waiting, None),
            ReviewFixtureState::Approved => (StageRunStatus::Succeeded, Some(1_800_000_000_030)),
            ReviewFixtureState::ChangesRequested | ReviewFixtureState::Rejected => {
                (StageRunStatus::Failed, Some(1_800_000_000_030))
            }
        };
        snapshot.stage_runs.push(StageRun {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: StageRunId("stage:plan-review".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: None,
            stage: DeliveryStage::PlanReview,
            actor_type: StageRunActorType::Human,
            role: "reviewer".into(),
            status: review_status,
            attempt: 1,
            started_at_millis: 1_800_000_000_021,
            finished_at_millis: finished_at,
        });

        let context = context_for(&snapshot);
        let pending = state == ReviewFixtureState::Pending;
        let attention_status = match state {
            ReviewFixtureState::Pending => AttentionItemStatus::Open,
            ReviewFixtureState::Approved => AttentionItemStatus::Resolved,
            ReviewFixtureState::ChangesRequested | ReviewFixtureState::Rejected => {
                AttentionItemStatus::Dismissed
            }
        };
        snapshot.attention_items.push(AttentionItem {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: context.attention_item_id.clone(),
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            stage_run_id: Some(context.review_stage_run_id.clone()),
            item_type: AttentionItemType::DecisionRequired,
            title: "Review delivery solution".into(),
            context: serde_json::to_string(&context).expect("context JSON"),
            options: vec![],
            assigned_to: Some("alice".into()),
            blocking: true,
            status: attention_status,
            resolution: (!pending).then(|| {
                serde_json::to_string(&decision_for(&context, state)).expect("decision JSON")
            }),
            resolved_by: (!pending).then(|| "alice".into()),
            created_at_millis: context.prepared_at,
            resolved_at_millis: (!pending).then_some(1_800_000_000_030),
        });
        snapshot.updated_at_millis = 1_800_000_000_031;
        Delivery::try_from_snapshot(snapshot).expect("solution-review Delivery")
    }

    pub(crate) fn with_newer_review_attempt(
        history: Delivery,
        current: ReviewFixtureState,
    ) -> Delivery {
        let mut snapshot = history.into_snapshot();
        snapshot.status = match current {
            ReviewFixtureState::Pending => DeliveryStatus::NeedsAttention,
            ReviewFixtureState::Approved => DeliveryStatus::Executing,
            ReviewFixtureState::ChangesRequested => DeliveryStatus::Planning,
            ReviewFixtureState::Rejected => DeliveryStatus::Clarifying,
        };

        let planning_stage_run_id = StageRunId("stage:planning-2".into());
        let planning_session_binding_id = SessionBindingId("binding:planning-2".into());
        let review_stage_run_id = StageRunId("stage:plan-review-2".into());
        let attention_item_id = AttentionItemId("attention:plan-review-2".into());
        snapshot.stage_runs.push(StageRun {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: planning_stage_run_id.clone(),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: None,
            stage: DeliveryStage::Planning,
            actor_type: StageRunActorType::Codex,
            role: "planner".into(),
            status: StageRunStatus::Succeeded,
            attempt: 2,
            started_at_millis: 1_800_000_000_040,
            finished_at_millis: Some(1_800_000_000_050),
        });
        snapshot.session_bindings.push(SessionBinding {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: planning_session_binding_id.clone(),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: None,
            stage_run_id: planning_stage_run_id.clone(),
            product_session_id: ProductSessionId("product:planning-2".into()),
            execution_job_id: ExecutionJobId("job:planning-2".into()),
            worker_session_id: Some(WorkerSessionId("worker-session:planning-2".into())),
            codex_thread_id: Some(CodexThreadId("codex-thread:planning-2".into())),
            bound_at_millis: 1_800_000_000_041,
        });
        let (review_status, finished_at_millis) = match current {
            ReviewFixtureState::Pending => (StageRunStatus::Waiting, None),
            ReviewFixtureState::Approved => (StageRunStatus::Succeeded, Some(1_800_000_000_060)),
            ReviewFixtureState::ChangesRequested | ReviewFixtureState::Rejected => {
                (StageRunStatus::Failed, Some(1_800_000_000_060))
            }
        };
        snapshot.stage_runs.push(StageRun {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: review_stage_run_id.clone(),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: None,
            stage: DeliveryStage::PlanReview,
            actor_type: StageRunActorType::Human,
            role: "reviewer".into(),
            status: review_status,
            attempt: 2,
            started_at_millis: 1_800_000_000_051,
            finished_at_millis,
        });

        let context = context_for_attempt(
            &snapshot,
            planning_stage_run_id,
            planning_session_binding_id,
            review_stage_run_id.clone(),
            attention_item_id.clone(),
            1_800_000_000_049,
        );
        let pending = current == ReviewFixtureState::Pending;
        snapshot.attention_items.push(AttentionItem {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: attention_item_id,
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            stage_run_id: Some(review_stage_run_id),
            item_type: AttentionItemType::DecisionRequired,
            title: "Review revised delivery solution".into(),
            context: serde_json::to_string(&context).expect("new context JSON"),
            options: vec![],
            assigned_to: Some("alice".into()),
            blocking: true,
            status: match current {
                ReviewFixtureState::Pending => AttentionItemStatus::Open,
                ReviewFixtureState::Approved => AttentionItemStatus::Resolved,
                ReviewFixtureState::ChangesRequested | ReviewFixtureState::Rejected => {
                    AttentionItemStatus::Dismissed
                }
            },
            resolution: (!pending).then(|| {
                serde_json::to_string(&decision_for(&context, current)).expect("new decision JSON")
            }),
            resolved_by: (!pending).then(|| "alice".into()),
            created_at_millis: context.prepared_at,
            resolved_at_millis: (!pending).then_some(1_800_000_000_060),
        });
        snapshot.updated_at_millis = 1_800_000_000_061;
        Delivery::try_from_snapshot(snapshot).expect("Delivery with review history")
    }

    fn pending_resolution_input(
        delivery: &Delivery,
        state: ReviewFixtureState,
    ) -> ResolveAttentionInput {
        let attention = delivery
            .snapshot()
            .attention_items
            .iter()
            .filter(|item| item.status == AttentionItemStatus::Open)
            .max_by_key(|item| item.created_at_millis)
            .expect("current pending review Attention");
        let context: SolutionReviewContextV1 =
            serde_json::from_str(&attention.context).expect("pending context");
        ResolveAttentionInput {
            expected_revision: delivery.revision(),
            attention_item_id: attention.id.clone(),
            stage_run_id: attention.stage_run_id.clone().expect("review StageRun"),
            expected_context: attention.context.clone(),
            actor: "alice".into(),
            decision: if state == ReviewFixtureState::Approved {
                AttentionDecision::Resolved
            } else {
                AttentionDecision::Dismissed
            },
            resolution: serde_json::to_string(&decision_for(&context, state))
                .expect("canonical decision"),
            now_millis: delivery.snapshot().updated_at_millis + 1,
        }
    }

    fn rewrite_context(
        delivery: Delivery,
        rewrite: impl FnOnce(&mut SolutionReviewContextV1),
    ) -> Delivery {
        let mut snapshot = delivery.into_snapshot();
        let mut context: SolutionReviewContextV1 =
            serde_json::from_str(&snapshot.attention_items[0].context).expect("context");
        rewrite(&mut context);
        context.review_set_sha256 = review_set_digest(&context).expect("rewritten digest");
        snapshot.attention_items[0].context =
            serde_json::to_string(&context).expect("rewritten context");
        if let Some(resolution) = snapshot.attention_items[0].resolution.as_deref() {
            let mut decision: SolutionReviewDecisionV1 =
                serde_json::from_str(resolution).expect("decision");
            decision.review_set_sha256 = context.review_set_sha256;
            snapshot.attention_items[0].resolution =
                Some(serde_json::to_string(&decision).expect("rewritten decision"));
        }
        Delivery::try_from_snapshot(snapshot).expect("rewritten Delivery")
    }

    #[test]
    fn pending_solution_review_projects_safe_solution_and_non_empty_ordered_task_proposals() {
        let delivery = review_delivery(ReviewFixtureState::Pending);
        let projection = project_delivery_detail(ProjectionInput::new(&delivery))
            .expect("pending solution review projection");
        let solution_review = projection.solution_review().expect("solution review");

        assert_eq!(
            solution_review.review_status(),
            SolutionReviewStatusProjection::Pending
        );
        assert!(solution_review.decision().is_none());
        assert!(solution_review.reviewer_id().is_none());
        assert!(solution_review.reviewed_at().is_none());
        assert!(!solution_review.task_proposals().is_empty());
        assert_eq!(
            solution_review.task_proposals()[0].id().0,
            "task:invitation"
        );
        let wire = serde_json::to_value(&projection).expect("projection JSON");
        assert!(wire.get("solutionReview").is_some());
        assert!(wire.get("solution").is_none());
    }

    #[test]
    fn settled_solution_review_projects_exact_decision_reviewer_and_time() {
        let cases = [
            (
                ReviewFixtureState::Approved,
                SolutionReviewStatusProjection::Approved,
                SolutionReviewDecisionProjection::Approve,
            ),
            (
                ReviewFixtureState::ChangesRequested,
                SolutionReviewStatusProjection::ChangesRequested,
                SolutionReviewDecisionProjection::RequestChanges,
            ),
            (
                ReviewFixtureState::Rejected,
                SolutionReviewStatusProjection::Rejected,
                SolutionReviewDecisionProjection::Reject,
            ),
        ];
        for (fixture_state, expected_status, expected_decision) in cases {
            let delivery = review_delivery(fixture_state);
            let projection = project_delivery_detail(ProjectionInput::new(&delivery))
                .expect("settled solution review projection");
            let solution_review = projection.solution_review().expect("solution review");
            assert_eq!(solution_review.review_status(), expected_status);
            assert_eq!(solution_review.decision(), Some(expected_decision));
            let reviewer_id = solution_review.reviewer_id();
            let reviewed_at = solution_review.reviewed_at();
            assert_eq!(reviewer_id, Some("alice"));
            assert_eq!(reviewed_at, Some(1_800_000_000_030));
        }
    }

    #[test]
    fn only_approved_solution_review_authorizes_task_promotion() {
        let approved_delivery = review_delivery(ReviewFixtureState::Approved);
        let pending = review_delivery(ReviewFixtureState::Pending);
        let changes_requested = review_delivery(ReviewFixtureState::ChangesRequested);
        let rejected = review_delivery(ReviewFixtureState::Rejected);

        let approved = resolve_current_solution_review(&approved_delivery)
            .expect("approved resolver")
            .expect("approved review");
        let promotion = approved.approved_task_promotion().expect("promotion");
        assert_eq!(promotion.review_set_sha256(), approved.review_set_sha256);
        assert_eq!(promotion.task_proposals().len(), 1);
        for delivery in [&pending, &changes_requested, &rejected] {
            let review = resolve_current_solution_review(delivery)
                .expect("review resolver")
                .expect("review fact");
            assert!(review.approved_task_promotion().is_none());
        }
    }

    #[test]
    fn solution_review_v1_rejects_unknown_keys_and_legacy_v2() {
        let delivery = review_delivery(ReviewFixtureState::Pending);
        let mut unknown = delivery.clone().into_snapshot();
        let mut context: Value =
            serde_json::from_str(&unknown.attention_items[0].context).expect("context Value");
        context["unexpected"] = json!(true);
        unknown.attention_items[0].context = context.to_string();
        let unknown = Delivery::try_from_snapshot(unknown).expect("unknown-key Delivery");
        assert!(resolve_current_solution_review(&unknown).is_err());

        let legacy_v2 = ["winwincode", "plan-review-context", "v2"].join(".");
        let legacy = rewrite_context(delivery, |context| context.protocol = legacy_v2);
        assert!(resolve_current_solution_review(&legacy).is_err());

        let mut decision_legacy = review_delivery(ReviewFixtureState::Approved).into_snapshot();
        let mut resolution: Value = serde_json::from_str(
            decision_legacy.attention_items[0]
                .resolution
                .as_deref()
                .expect("resolution"),
        )
        .expect("decision Value");
        resolution["protocol"] = json!(["winwincode", "plan-review-decision", "v2"].join("."));
        decision_legacy.attention_items[0].resolution = Some(resolution.to_string());
        let decision_legacy =
            Delivery::try_from_snapshot(decision_legacy).expect("legacy decision Delivery");
        assert!(resolve_current_solution_review(&decision_legacy).is_err());
    }

    #[test]
    fn solution_review_resolver_rejects_stale_or_foreign_authority() {
        let stale = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            context.delivery_spec_revision += 1;
        });
        assert!(resolve_current_solution_review(&stale).is_err());

        let foreign = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            context.planning_session_binding_id = SessionBindingId("binding:foreign".into());
        });
        assert!(resolve_current_solution_review(&foreign).is_err());

        let mut foreign_actor = review_delivery(ReviewFixtureState::Pending).into_snapshot();
        foreign_actor
            .stage_runs
            .iter_mut()
            .find(|run| run.stage == DeliveryStage::PlanReview)
            .expect("review StageRun")
            .actor_type = StageRunActorType::Codex;
        let foreign_actor =
            Delivery::try_from_snapshot(foreign_actor).expect("foreign actor Delivery");
        assert!(resolve_current_solution_review(&foreign_actor).is_err());

        let mut wrong_reviewer = review_delivery(ReviewFixtureState::Approved).into_snapshot();
        wrong_reviewer.attention_items[0].resolved_by = Some("mallory".into());
        let wrong_reviewer =
            Delivery::try_from_snapshot(wrong_reviewer).expect("wrong reviewer Delivery");
        assert!(resolve_current_solution_review(&wrong_reviewer).is_err());

        let mut wrong_time = review_delivery(ReviewFixtureState::Approved).into_snapshot();
        wrong_time.attention_items[0].resolved_at_millis = Some(1_800_000_000_031);
        let wrong_time = Delivery::try_from_snapshot(wrong_time).expect("wrong time Delivery");
        assert!(resolve_current_solution_review(&wrong_time).is_err());
    }

    #[test]
    fn solution_review_digest_covers_ordered_task_proposals() {
        let delivery = review_delivery(ReviewFixtureState::Pending);
        let mut context: SolutionReviewContextV1 =
            serde_json::from_str(&delivery.snapshot().attention_items[0].context).expect("context");
        let first_task = context.task_proposals[0].clone();
        context.task_proposals.push(DeliveryTaskProposal {
            id: DeliveryTaskId("task:verification".into()),
            title: "Verify invitation flow".into(),
            goal: "Run the exact acceptance checks.".into(),
            acceptance_criterion_ids: first_task.acceptance_criterion_ids.clone(),
            blocked_by_task_ids: vec![first_task.id.clone()],
        });
        let original_order_digest = review_set_digest(&context).expect("ordered digest");
        context.task_proposals.swap(0, 1);
        let reversed_order_digest = review_set_digest(&context).expect("reversed digest");
        assert_ne!(original_order_digest, reversed_order_digest);

        let mut stale_order = delivery.into_snapshot();
        context.review_set_sha256 = original_order_digest;
        stale_order.attention_items[0].context =
            serde_json::to_string(&context).expect("stale-order context");
        let stale_order = Delivery::try_from_snapshot(stale_order).expect("stale-order Delivery");
        assert!(resolve_current_solution_review(&stale_order).is_err());
    }

    pub(crate) fn empty_task_proposals_fixture() -> Delivery {
        rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            context.task_proposals.clear();
        })
    }

    pub(crate) fn ordered_task_proposals_fixture() -> Delivery {
        rewrite_context(review_delivery(ReviewFixtureState::Approved), |context| {
            let second_criterion = context.task_proposals[0]
                .acceptance_criterion_ids
                .pop()
                .expect("second acceptance criterion");
            let first_id = context.task_proposals[0].id.clone();
            context.task_proposals.push(DeliveryTaskProposal {
                id: DeliveryTaskId("task:verification".into()),
                title: "Verify invitation flow".into(),
                goal: "Run the exact acceptance checks.".into(),
                acceptance_criterion_ids: vec![second_criterion],
                blocked_by_task_ids: vec![first_id],
            });
        })
    }

    pub(crate) fn duplicate_task_and_criterion_fixtures() -> (Delivery, Delivery) {
        let duplicate = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            context
                .task_proposals
                .push(context.task_proposals[0].clone());
        });
        let duplicate_criterion =
            rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
                let criterion = context.task_proposals[0].acceptance_criterion_ids[0].clone();
                context.task_proposals[0]
                    .acceptance_criterion_ids
                    .push(criterion);
            });
        (duplicate, duplicate_criterion)
    }

    pub(crate) fn invalid_criterion_fixtures() -> [Delivery; 3] {
        let empty = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            context.task_proposals[0].acceptance_criterion_ids.clear();
        });
        let foreign = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            context.task_proposals[0].acceptance_criterion_ids =
                vec![AcceptanceCriterionId("criterion:foreign".into())];
        });
        let incomplete = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            context.task_proposals[0]
                .acceptance_criterion_ids
                .pop()
                .expect("second acceptance criterion");
        });
        [empty, foreign, incomplete]
    }

    pub(crate) fn invalid_dependency_fixtures() -> [Delivery; 4] {
        let self_dependency =
            rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
                context.task_proposals[0].blocked_by_task_ids =
                    vec![context.task_proposals[0].id.clone()];
            });
        let missing = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            context.task_proposals[0].blocked_by_task_ids =
                vec![DeliveryTaskId("task:missing".into())];
        });
        let duplicate = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            let dependency = DeliveryTaskProposal {
                id: DeliveryTaskId("task:dependency".into()),
                title: "Dependency".into(),
                goal: "Retain current acceptance coverage.".into(),
                acceptance_criterion_ids: context.task_proposals[0]
                    .acceptance_criterion_ids
                    .clone(),
                blocked_by_task_ids: vec![],
            };
            context.task_proposals[0].blocked_by_task_ids =
                vec![dependency.id.clone(), dependency.id.clone()];
            context.task_proposals.push(dependency);
        });
        let cycle = rewrite_context(review_delivery(ReviewFixtureState::Pending), |context| {
            let first_id = context.task_proposals[0].id.clone();
            let second_id = DeliveryTaskId("task:cycle".into());
            context.task_proposals[0].blocked_by_task_ids = vec![second_id.clone()];
            context.task_proposals.push(DeliveryTaskProposal {
                id: second_id,
                title: "Cycle".into(),
                goal: "Exercise cycle rejection.".into(),
                acceptance_criterion_ids: context.task_proposals[0]
                    .acceptance_criterion_ids
                    .clone(),
                blocked_by_task_ids: vec![first_id],
            });
        });
        [self_dependency, missing, duplicate, cycle]
    }

    #[test]
    fn human_solution_review_rejects_execution_session_binding() {
        let mut snapshot = review_delivery(ReviewFixtureState::Pending).into_snapshot();
        snapshot.session_bindings.push(SessionBinding {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId("binding:forged-human-review".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: None,
            stage_run_id: StageRunId("stage:plan-review".into()),
            product_session_id: ProductSessionId("product:forged-human-review".into()),
            execution_job_id: ExecutionJobId("job:forged-human-review".into()),
            worker_session_id: Some(WorkerSessionId("worker:forged-human-review".into())),
            codex_thread_id: Some(CodexThreadId("thread:forged-human-review".into())),
            bound_at_millis: 1_800_000_000_022,
        });
        assert!(Delivery::try_from_snapshot(snapshot).is_err());
    }

    #[test]
    fn solution_review_projection_excludes_raw_context_resolution_secrets_tools_and_logs() {
        let delivery = review_delivery(ReviewFixtureState::Approved);
        let raw_context = delivery.snapshot().attention_items[0].context.clone();
        let raw_resolution = delivery.snapshot().attention_items[0]
            .resolution
            .clone()
            .expect("resolution");
        let projection =
            project_delivery_detail(ProjectionInput::new(&delivery)).expect("safe projection");
        let encoded = String::from_utf8(projection.encode_json().expect("projection JSON"))
            .expect("UTF-8 projection");
        for secret in [
            "credential",
            "authorization",
            "toolPayload",
            "rawRuntimeLog",
        ] {
            assert!(!encoded.contains(secret));
        }

        let mut poisoned_context = delivery.clone().into_snapshot();
        let mut context: Value = serde_json::from_str(&raw_context).expect("context Value");
        context["secret"] = json!("must-not-leak");
        context["toolPayload"] = json!({ "command": "hidden" });
        context["rawRuntimeLog"] = json!("hidden");
        poisoned_context.attention_items[0].context = context.to_string();
        let poisoned_context =
            Delivery::try_from_snapshot(poisoned_context).expect("poisoned context Delivery");
        assert!(resolve_current_solution_review(&poisoned_context).is_err());

        let mut poisoned_resolution = delivery.into_snapshot();
        let mut resolution: Value =
            serde_json::from_str(&raw_resolution).expect("resolution Value");
        resolution["authorization"] = json!("must-not-leak");
        poisoned_resolution.attention_items[0].resolution = Some(resolution.to_string());
        let poisoned_resolution =
            Delivery::try_from_snapshot(poisoned_resolution).expect("poisoned resolution Delivery");
        assert!(resolve_current_solution_review(&poisoned_resolution).is_err());
    }

    #[test]
    fn solution_review_resolver_rejects_duplicate_current_review() {
        let mut snapshot = review_delivery(ReviewFixtureState::Pending).into_snapshot();
        let first_review = snapshot
            .stage_runs
            .iter()
            .find(|run| run.stage == DeliveryStage::PlanReview)
            .expect("review StageRun")
            .clone();
        let mut duplicate_review = first_review;
        duplicate_review.id = StageRunId("stage:plan-review-duplicate".into());
        snapshot.stage_runs.push(duplicate_review);

        let mut context: SolutionReviewContextV1 =
            serde_json::from_str(&snapshot.attention_items[0].context).expect("context");
        context.review_stage_run_id = StageRunId("stage:plan-review-duplicate".into());
        context.attention_item_id = AttentionItemId("attention:plan-review-duplicate".into());
        context.review_set_sha256 = review_set_digest(&context).expect("duplicate digest");
        let mut duplicate_attention = snapshot.attention_items[0].clone();
        duplicate_attention.id = context.attention_item_id.clone();
        duplicate_attention.stage_run_id = Some(context.review_stage_run_id.clone());
        duplicate_attention.context = serde_json::to_string(&context).expect("duplicate context");
        snapshot.attention_items.push(duplicate_attention);
        let duplicate = Delivery::try_from_snapshot(snapshot).expect("duplicate review Delivery");

        let error = resolve_current_solution_review(&duplicate).expect_err("duplicate review");
        assert_eq!(
            error.code(),
            SolutionReviewErrorCode::AmbiguousCurrentReview
        );

        let mut duplicate_attention = review_delivery(ReviewFixtureState::Pending).into_snapshot();
        let mut second = duplicate_attention.attention_items[0].clone();
        let mut second_context: SolutionReviewContextV1 =
            serde_json::from_str(&second.context).expect("second context");
        second_context.attention_item_id = AttentionItemId("attention:plan-review-2".into());
        second_context.review_set_sha256 =
            review_set_digest(&second_context).expect("second context digest");
        second.id = second_context.attention_item_id.clone();
        second.context = serde_json::to_string(&second_context).expect("second context JSON");
        duplicate_attention.attention_items.push(second);
        let duplicate_attention =
            Delivery::try_from_snapshot(duplicate_attention).expect("duplicate Attention Delivery");
        assert_eq!(
            resolve_current_solution_review(&duplicate_attention)
                .expect_err("same review run cannot have two current Attention items")
                .code(),
            SolutionReviewErrorCode::AmbiguousCurrentReview
        );
    }

    #[test]
    fn newer_solution_review_attempt_supersedes_settled_history() {
        for historical_state in [
            ReviewFixtureState::ChangesRequested,
            ReviewFixtureState::Rejected,
        ] {
            for current_state in [ReviewFixtureState::Pending, ReviewFixtureState::Approved] {
                let delivery =
                    with_newer_review_attempt(review_delivery(historical_state), current_state);
                let review = resolve_current_solution_review(&delivery)
                    .expect("lower-attempt history must be ignored")
                    .expect("current review");
                assert_eq!(review.review_stage_run_id.0, "stage:plan-review-2");
                assert_eq!(
                    review.review_status,
                    if current_state == ReviewFixtureState::Pending {
                        ValidatedReviewStatus::Pending
                    } else {
                        ValidatedReviewStatus::Approved
                    }
                );
            }
        }
    }

    #[test]
    fn old_solution_review_cannot_authorize_promotion_after_a_new_attempt() {
        let historical = review_delivery(ReviewFixtureState::Approved);
        let old_review = resolve_current_solution_review(&historical)
            .expect("historical approved review")
            .expect("historical review fact");
        let old_promotion = old_review
            .approved_task_promotion()
            .expect("historical promotion seal");
        let current = with_newer_review_attempt(historical, ReviewFixtureState::Pending);

        assert!(old_promotion.validate_for_delivery(&current).is_err());
    }

    #[test]
    fn approved_task_promotion_seal_binds_the_review_actor_and_time() {
        let historical = review_delivery(ReviewFixtureState::Approved);
        let old_review = resolve_current_solution_review(&historical)
            .expect("historical approved review")
            .expect("historical review fact");
        let old_promotion = old_review
            .approved_task_promotion()
            .expect("historical promotion seal");

        let mut snapshot = historical.into_snapshot();
        let changed_reviewed_at = snapshot.attention_items[0]
            .resolved_at_millis
            .expect("review time")
            + 1;
        snapshot.attention_items[0].assigned_to = Some("bob".into());
        snapshot.attention_items[0].resolved_by = Some("bob".into());
        snapshot.attention_items[0].resolved_at_millis = Some(changed_reviewed_at);
        snapshot
            .stage_runs
            .iter_mut()
            .find(|run| run.stage == DeliveryStage::PlanReview)
            .expect("review StageRun")
            .finished_at_millis = Some(changed_reviewed_at);
        snapshot.updated_at_millis = changed_reviewed_at;
        let changed = Delivery::try_from_snapshot(snapshot).expect("changed review settlement");
        assert!(
            resolve_current_solution_review(&changed)
                .expect("changed review is internally valid")
                .is_some()
        );

        assert!(old_promotion.validate_for_delivery(&changed).is_err());
    }

    #[test]
    fn typed_solution_review_resolution_drives_exact_delivery_state() {
        let cases = [
            (ReviewFixtureState::Approved, DeliveryStatus::Executing),
            (
                ReviewFixtureState::ChangesRequested,
                DeliveryStatus::Planning,
            ),
            (ReviewFixtureState::Rejected, DeliveryStatus::Clarifying),
        ];
        for (state, expected_status) in cases {
            let pending = review_delivery(ReviewFixtureState::Pending);
            let input = pending_resolution_input(&pending, state);
            let transition = resolve_attention(&pending, input.clone())
                .expect("typed plan-review decision must settle atomically");
            assert_eq!(transition.snapshot().status, expected_status);
            let settled = resolve_current_solution_review(&transition)
                .expect("settled Delivery must remain projectable")
                .expect("settled review");
            assert_eq!(settled.reviewer_id.as_deref(), Some("alice"));

            let repeated = resolve_attention(&pending, input)
                .expect("same source and input produce the same sealed transition");
            assert_eq!(transition.into_delivery(), repeated.into_delivery());
        }
    }

    #[test]
    fn typed_solution_review_resolution_rejects_raw_stale_or_foreign_decision() {
        let pending = review_delivery(ReviewFixtureState::Pending);

        let mut raw = pending_resolution_input(&pending, ReviewFixtureState::Approved);
        raw.resolution = "approve".into();
        assert!(resolve_attention(&pending, raw).is_err());

        let mut stale = pending_resolution_input(&pending, ReviewFixtureState::Approved);
        let mut stale_value: Value =
            serde_json::from_str(&stale.resolution).expect("decision Value");
        stale_value["reviewSetSha256"] = json!("0".repeat(64));
        stale.resolution = serde_json::to_string(&stale_value).expect("stale decision JSON");
        assert!(resolve_attention(&pending, stale).is_err());

        let mut foreign_actor = pending_resolution_input(&pending, ReviewFixtureState::Approved);
        foreign_actor.actor = "mallory".into();
        assert!(resolve_attention(&pending, foreign_actor).is_err());
    }

    #[test]
    fn typed_solution_review_resolution_replays_once_in_the_delivery_store() {
        let pending = review_delivery(ReviewFixtureState::Pending);
        let mut seeded_snapshot = pending.clone().into_snapshot();
        seeded_snapshot.revision = 1;
        let seeded = Delivery::try_from_snapshot(seeded_snapshot).expect("revision-one review");
        let transition = resolve_attention(
            &seeded,
            pending_resolution_input(&seeded, ReviewFixtureState::Approved),
        )
        .expect("typed approval transition");

        let backend = Arc::new(InMemoryDeliveryJournal::new());
        let store = DeliveryStore::new(backend);
        store
            .execute(DeliveryCommand::SeedForTest(CreateDelivery {
                request_id: RequestId("seed-solution-review".into()),
                request_digest: "a".repeat(64),
                snapshot: seeded.clone(),
            }))
            .expect("seed pending review");

        let raw = store
            .execute(DeliveryCommand::Append(AppendDelivery {
                delivery_id: seeded.id().clone(),
                request_id: RequestId("raw-solution-review".into()),
                request_digest: "b".repeat(64),
                operation: DeliveryMutationOperation::AttentionResolved,
                expected_revision: seeded.revision(),
                snapshot: transition.delivery().clone(),
            }))
            .expect_err("raw append cannot persist a plan-review decision");
        assert_eq!(raw.code(), DeliveryStoreErrorCode::InvalidStoreOptions);

        let command = || {
            DeliveryCommand::ResolveAttention(Box::new(ResolveDeliveryAttention {
                request_id: RequestId("resolve-solution-review".into()),
                request_digest: "c".repeat(64),
                expected_revision: seeded.revision(),
                transition: transition.clone(),
            }))
        };
        let first = store.execute(command()).expect("first atomic settlement");
        let replay = store.execute(command()).expect("idempotent replay");
        assert!(!first.replayed);
        assert!(replay.replayed);
        assert_eq!(first.snapshot, replay.snapshot);
    }

    #[test]
    fn solution_review_v1_rejects_noncanonical_or_duplicate_json() {
        let pending = review_delivery(ReviewFixtureState::Pending);
        let canonical_context = &pending.snapshot().attention_items[0].context;

        let mut whitespace = pending.clone().into_snapshot();
        whitespace.attention_items[0].context = format!(" {canonical_context}");
        let whitespace = Delivery::try_from_snapshot(whitespace).expect("whitespace Delivery");
        assert!(resolve_current_solution_review(&whitespace).is_err());

        let mut reordered = pending.clone().into_snapshot();
        let context_value: Value =
            serde_json::from_str(canonical_context).expect("canonical context Value");
        reordered.attention_items[0].context = context_value.to_string();
        let reordered = Delivery::try_from_snapshot(reordered).expect("reordered Delivery");
        assert!(resolve_current_solution_review(&reordered).is_err());

        let digest = serde_json::from_str::<SolutionReviewContextV1>(canonical_context)
            .expect("context")
            .review_set_sha256;
        let duplicate_digest = canonical_context.replacen(
            &format!("\"reviewSetSha256\":\"{digest}\""),
            &format!("\"reviewSetSha256\":\"{digest}\",\"reviewSetSha256\":\"{digest}\""),
            1,
        );
        let mut duplicate = pending.clone().into_snapshot();
        duplicate.attention_items[0].context = duplicate_digest;
        let duplicate = Delivery::try_from_snapshot(duplicate).expect("duplicate-key Delivery");
        assert!(resolve_current_solution_review(&duplicate).is_err());

        let approved = review_delivery(ReviewFixtureState::Approved);
        let canonical_decision = approved.snapshot().attention_items[0]
            .resolution
            .as_deref()
            .expect("canonical decision");
        for malformed in [
            format!(" {canonical_decision}"),
            canonical_decision.replacen(
                "\"action\":\"approve\"",
                "\"action\":\"approve\",\"action\":\"approve\"",
                1,
            ),
            canonical_decision.replacen(
                "\"comments\":",
                "\"unknown\":true,\"unknown\":true,\"comments\":",
                1,
            ),
        ] {
            let mut snapshot = approved.clone().into_snapshot();
            snapshot.attention_items[0].resolution = Some(malformed);
            let malformed =
                Delivery::try_from_snapshot(snapshot).expect("malformed decision Delivery");
            assert!(resolve_current_solution_review(&malformed).is_err());
        }
    }

    #[test]
    fn solution_review_v1_restart_parse_is_byte_stable() {
        let delivery = review_delivery(ReviewFixtureState::Pending);
        let encoded = delivery.snapshot().attention_items[0].context.as_bytes();
        let decoded: SolutionReviewContextV1 =
            serde_json::from_slice(encoded).expect("decode context before restart");
        let restart_bytes = serde_json::to_vec(&decoded).expect("encode context after restart");
        let reparsed: SolutionReviewContextV1 =
            serde_json::from_slice(&restart_bytes).expect("reparse context after restart");

        assert_eq!(encoded, restart_bytes);
        assert_eq!(
            review_set_digest(&decoded).expect("first digest"),
            review_set_digest(&reparsed).expect("restart digest")
        );
    }

    fn planning_handoff_fixture(
        now_millis: u64,
    ) -> (Delivery, crate::application::stage::AdvanceStageInput) {
        use crate::application::stage::{
            AdvanceStageInput, NewStageIdentities, TerminalOutcomeStatus,
            test_support::{
                active_lease_identity, terminal_outcome_metadata, terminal_worker_outcome,
                verify_terminal_outcome,
            },
        };

        let mut snapshot = review_delivery(ReviewFixtureState::Pending).into_snapshot();
        snapshot.status = DeliveryStatus::Planning;
        snapshot.stage_runs.truncate(1);
        snapshot.stage_runs[0].status = StageRunStatus::Running;
        snapshot.stage_runs[0].finished_at_millis = None;
        snapshot.attention_items.clear();
        snapshot.updated_at_millis = snapshot.stage_runs[0].started_at_millis;
        let delivery = Delivery::try_from_snapshot(snapshot).expect("active planning Delivery");
        let run = &delivery.snapshot().stage_runs[0];
        let binding = &delivery.snapshot().session_bindings[0];
        let worker_session_id = binding
            .worker_session_id
            .clone()
            .expect("planning WorkerSession");
        let lease = active_lease_identity(
            binding.execution_job_id.clone(),
            run.attempt,
            LeaseId("lease:planning".into()),
            FencingToken("1".into()),
            WorkerId("worker:planning".into()),
            WorkerInstanceId("worker-instance:planning".into()),
            worker_session_id.clone(),
        );
        let metadata = terminal_outcome_metadata(
            binding.codex_thread_id.clone(),
            now_millis,
            ExecutionAckSequence(9),
            Vec::new(),
        );
        let raw = terminal_worker_outcome(
            run.id.clone(),
            binding.execution_job_id.clone(),
            run.attempt,
            lease.lease_id().clone(),
            lease.fencing_token().clone(),
            lease.worker_id().clone(),
            lease.worker_instance_id().clone(),
            worker_session_id,
            TerminalOutcomeStatus::Succeeded,
            metadata,
        );
        let terminal =
            verify_terminal_outcome(&delivery, &lease, raw).expect("verified planning outcome");
        let input = AdvanceStageInput {
            expected_revision: delivery.revision(),
            product_session_id: ProductSessionId("product:review-unused".into()),
            identities: NewStageIdentities {
                stage_run_id: StageRunId("stage:plan-review-fixture".into()),
                execution_job_id: ExecutionJobId("job:review-unused".into()),
                session_binding_id: SessionBindingId("binding:review-unused".into()),
                attention_item_id: AttentionItemId("attention:plan-review-fixture".into()),
            },
            review: None,
            previous_outcome: Some(terminal),
            current_lease: Some(lease),
            rework_authorization: None,
            now_millis,
        };
        (delivery, input)
    }

    fn semantic_review_fixture(
        task_proposals: Vec<test_support::SolutionReviewTaskProposalFixture>,
    ) -> test_support::SolutionReviewFixture {
        use test_support::{
            SolutionComponentFixture, SolutionComponentKindFixture, SolutionConnectionFixture,
            SolutionDiagramEdgeFixture, SolutionDiagramFixture, SolutionDiagramKindFixture,
            SolutionDiagramNodeFixture, SolutionDiagramNodeKindFixture, SolutionFixture,
            SolutionReviewFixture,
        };

        let diagram = |id: &str, kind| SolutionDiagramFixture {
            id: id.to_owned(),
            kind,
            title: format!("{id} fixture"),
            nodes: vec![
                SolutionDiagramNodeFixture {
                    id: format!("{id}:input"),
                    label: "Input".into(),
                    description: "Starts the fixture flow.".into(),
                    kind: SolutionDiagramNodeKindFixture::Stage,
                    trust_boundary: None,
                    unresolved: false,
                },
                SolutionDiagramNodeFixture {
                    id: format!("{id}:output"),
                    label: "Output".into(),
                    description: "Completes the fixture flow.".into(),
                    kind: SolutionDiagramNodeKindFixture::Decision,
                    trust_boundary: Some("fixture-review".into()),
                    unresolved: false,
                },
            ],
            edges: vec![SolutionDiagramEdgeFixture {
                id: format!("{id}:edge"),
                from: format!("{id}:input"),
                to: format!("{id}:output"),
                label: "reviews".into(),
            }],
        };
        SolutionReviewFixture {
            attention_title: "Review the fixture solution".into(),
            assigned_to: "alice".into(),
            solution: SolutionFixture {
                id: "solution:fixture".into(),
                summary: "Exercise the canonical solution-review transition.".into(),
                approach: vec!["Preserve the exact current review set.".into()],
                components: vec![SolutionComponentFixture {
                    id: "component:fixture".into(),
                    label: "Fixture".into(),
                    responsibility: "Produces the expected Delivery result.".into(),
                    kind: SolutionComponentKindFixture::Component,
                    trust_boundary: Some("repository".into()),
                    unresolved: false,
                    repository_path_prefixes: vec!["src".into()],
                }],
                connections: vec![SolutionConnectionFixture {
                    id: "connection:fixture".into(),
                    from: "platform:codex-core".into(),
                    to: "component:fixture".into(),
                    label: "implements".into(),
                }],
            },
            architecture_diagram: diagram(
                "diagram:architecture-fixture",
                SolutionDiagramKindFixture::SystemArchitecture,
            ),
            process_diagram: diagram(
                "diagram:process-fixture",
                SolutionDiagramKindFixture::ProcessFlow,
            ),
            risks: vec!["A stale review must be rejected.".into()],
            unresolved_items: Vec::new(),
            task_proposals,
        }
    }

    #[test]
    fn high_level_solution_review_fixture_prepares_real_sealed_stage_transition() {
        let (delivery, input) = planning_handoff_fixture(1_800_000_000_020);
        let prepared = test_support::prepare_solution_review_fixture(
            &delivery,
            input,
            semantic_review_fixture(Vec::new()),
        )
        .expect("prepared solution review");

        prepared
            .transition()
            .validate_projection()
            .expect("sealed StageAdvanceResult");
        assert_eq!(prepared.review_set_sha256().len(), 64);
        let pending = resolve_current_solution_review(&prepared.transition().delivery)
            .expect("current review")
            .expect("pending review");
        assert_eq!(pending.review_status, ValidatedReviewStatus::Pending);
        assert_eq!(pending.review_set_sha256(), prepared.review_set_sha256());
        assert_eq!(pending.task_proposals.len(), 1);
        assert_eq!(
            pending.task_proposals[0].acceptance_criterion_ids,
            delivery
                .snapshot()
                .spec
                .acceptance_criteria
                .iter()
                .map(|criterion| criterion.id.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn high_level_solution_review_fixture_settles_and_promotes_exact_tasks() {
        let (delivery, input) = planning_handoff_fixture(1_800_000_000_020);
        let prepared = test_support::prepare_solution_review_fixture(
            &delivery,
            input,
            semantic_review_fixture(Vec::new()),
        )
        .expect("prepared solution review");
        let pending = prepared.transition().delivery.clone();
        let digest = prepared.review_set_sha256().to_owned();
        let settled = test_support::settle_solution_review_fixture(
            &pending,
            "alice",
            1_800_000_000_021,
            test_support::SolutionReviewDecisionFixture::Approve {
                comments: Some("Approved exact fixture set.".into()),
            },
        )
        .expect("settled solution review");

        assert_eq!(settled.review_set_sha256(), digest);
        let approved = resolve_current_solution_review(settled.transition().delivery())
            .expect("current review")
            .expect("approved review");
        assert_eq!(approved.review_status, ValidatedReviewStatus::Approved);
        let authority = approved
            .approved_task_promotion()
            .expect("approved promotion authority");
        let promotion = crate::application::task_breakdown::prepare_task_breakdown_promotion(
            settled.transition().delivery(),
            &authority,
        )
        .expect("real task promotion");
        assert_eq!(promotion.delivery().snapshot().tasks.len(), 1);
        assert_eq!(promotion.review_set_sha256(), digest);
    }

    #[test]
    fn high_level_solution_review_fixture_maps_non_approval_decisions() {
        let cases = [
            (
                test_support::SolutionReviewDecisionFixture::RequestChanges {
                    comments: Some("Add the missing boundary.".into()),
                    requested_changes: vec!["Add the exact stale-review test.".into()],
                },
                ValidatedReviewStatus::ChangesRequested,
                DeliveryStatus::Planning,
            ),
            (
                test_support::SolutionReviewDecisionFixture::Reject { comments: None },
                ValidatedReviewStatus::Rejected,
                DeliveryStatus::Clarifying,
            ),
        ];
        for (decision, expected_review, expected_delivery) in cases {
            let (delivery, input) = planning_handoff_fixture(1_800_000_000_020);
            let prepared = test_support::prepare_solution_review_fixture(
                &delivery,
                input,
                semantic_review_fixture(Vec::new()),
            )
            .expect("prepared solution review");
            let settled = test_support::settle_solution_review_fixture(
                &prepared.transition().delivery,
                "alice",
                1_800_000_000_021,
                decision,
            )
            .expect("settled solution review");
            let current = resolve_current_solution_review(settled.transition().delivery())
                .expect("current review")
                .expect("settled review");
            assert_eq!(current.review_status, expected_review);
            assert_eq!(
                settled.transition().delivery().snapshot().status,
                expected_delivery
            );
        }
    }

    #[test]
    fn high_level_solution_review_fixture_rejects_invalid_proposal_graphs() {
        use test_support::InvalidTaskProposalFixture::{
            DependencyCycle, DuplicateCriterionId, DuplicateTaskId, MissingDependency,
        };

        for invalid in [
            DependencyCycle,
            MissingDependency,
            DuplicateTaskId,
            DuplicateCriterionId,
        ] {
            let (delivery, input) = planning_handoff_fixture(1_800_000_000_020);
            let proposals = test_support::invalid_task_proposals_fixture(&delivery, invalid);
            let rejected = test_support::prepare_solution_review_fixture(
                &delivery,
                input,
                semantic_review_fixture(proposals),
            )
            .expect_err("invalid proposal graph must not produce sealed review authority");
            assert!(!rejected.message().is_empty());
        }
    }
}
