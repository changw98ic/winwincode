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
use winwincode_domain::{AttentionItemId, DeliveryId, WorkItemId, WorkRunId, WorkRunState};

use crate::domain::{
    AcceptanceCriterionId, AttentionItem, AttentionItemStatus, AttentionItemType, Delivery,
    DeliverySpecId, MAX_SAFE_INTEGER, SessionBindingId,
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
pub(crate) struct WorkItemProposal {
    id: WorkItemId,
    title: String,
    goal: String,
    criterion_ids: Vec<AcceptanceCriterionId>,
    depends_on: Vec<WorkItemId>,
}

impl WorkItemProposal {
    pub(crate) fn id(&self) -> &WorkItemId {
        &self.id
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn goal(&self) -> &str {
        &self.goal
    }

    pub(crate) fn criterion_ids(&self) -> &[AcceptanceCriterionId] {
        &self.criterion_ids
    }

    pub(crate) fn depends_on(&self) -> &[WorkItemId] {
        &self.depends_on
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
    planning_work_run_id: WorkRunId,
    planning_session_binding_id: SessionBindingId,
    attention_item_id: AttentionItemId,
    solution: SolutionWire,
    architecture_diagram: ValidatedDiagram,
    process_diagram: ValidatedDiagram,
    risks: Vec<String>,
    unresolved_items: Vec<String>,
    work_item_proposals: Vec<WorkItemProposal>,
    prepared_at: u64,
    review_set_sha256: String,
}

fn current_planning_authority(
    snapshot: &crate::domain::DeliverySnapshot,
    work_run_id: &WorkRunId,
) -> Result<(WorkRunId, SessionBindingId), SolutionReviewError> {
    let run = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| &run.id == work_run_id)
        .ok_or_else(|| stale_authority("explicit planning WorkRun is missing"))?;
    let bindings = snapshot
        .session_bindings
        .iter()
        .filter(|binding| &binding.work_run_id == work_run_id)
        .collect::<Vec<_>>();
    let [binding] = bindings.as_slice() else {
        return Err(stale_authority(
            "planning WorkRun requires one exact SessionBinding",
        ));
    };
    if binding.delivery_id != snapshot.id
        || binding.work_contract_id != run.work_contract_id
        || binding.work_contract_revision != run.contract_revision
        || binding.work_item_id != run.work_item_id
        || binding.work_item_revision != run.work_item_revision
        || binding.execution_job_id != run.execution_job_id
        || i64::try_from(binding.attempt).ok() != Some(run.attempt)
        || Some(&binding.product_session_id) != run.product_session_id.as_ref()
        || binding.worker_id.as_ref() != Some(&run.worker_id)
        || binding.worker_instance_id.as_ref() != Some(&run.worker_instance_id)
        || binding.worker_session_id.as_ref() != Some(&run.worker_session_id)
        || binding.lease_id.as_ref() != Some(&run.lease_id)
        || binding.fencing_token.as_ref().map(|token| token.0.as_str())
            != Some(run.fencing_token.as_str())
        || binding.codex_thread_id != run.codex_thread_id
        || binding.codex_thread_id.is_none()
        || !matches!(
            run.state,
            WorkRunState::CandidateReady | WorkRunState::Settled
        )
    {
        return Err(stale_authority(
            "planning WorkRun and accepted binding disagree",
        ));
    }
    Ok((run.id.clone(), binding.id.clone()))
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
    planning_work_run_id: &'context WorkRunId,
    planning_session_binding_id: &'context SessionBindingId,
    attention_item_id: &'context AttentionItemId,
    solution: &'context SolutionWire,
    architecture_diagram: &'context ValidatedDiagram,
    process_diagram: &'context ValidatedDiagram,
    risks: &'context [String],
    unresolved_items: &'context [String],
    work_item_proposals: &'context [WorkItemProposal],
    prepared_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedSolutionReviewSet {
    delivery_id: DeliveryId,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    planning_work_run_id: WorkRunId,
    planning_session_binding_id: SessionBindingId,
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
    work_item_proposals: Vec<WorkItemProposal>,
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
    pub(crate) planning_work_run_id: &'review WorkRunId,
    pub(crate) planning_session_binding_id: &'review SessionBindingId,
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
    pub(crate) work_item_proposals: &'review [WorkItemProposal],
    pub(crate) review_status: ValidatedReviewStatus,
    pub(crate) decision: Option<ValidatedReviewDecision>,
    pub(crate) comments: Option<&'review str>,
    pub(crate) requested_changes: Option<&'review [String]>,
    pub(crate) reviewer_id: Option<&'review str>,
    pub(crate) reviewed_at: Option<u64>,
}

impl ValidatedSolutionReviewSet {
    pub(crate) fn projection_view(&self) -> SolutionReviewView<'_> {
        SolutionReviewView {
            delivery_id: &self.delivery_id,
            delivery_spec_id: &self.delivery_spec_id,
            delivery_spec_revision: self.delivery_spec_revision,
            planning_work_run_id: &self.planning_work_run_id,
            planning_session_binding_id: &self.planning_session_binding_id,
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
            work_item_proposals: &self.work_item_proposals,
            review_status: self.review_status,
            decision: self.decision,
            comments: self.comments.as_deref(),
            requested_changes: self.requested_changes.as_deref(),
            reviewer_id: self.reviewer_id.as_deref(),
            reviewed_at: self.reviewed_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ValidatedSolutionReviewSettlement {
    resolve_attention: bool,
    attention_status: AttentionItemStatus,
    delivery_status: crate::domain::DeliveryStatus,
}

impl ValidatedSolutionReviewSettlement {
    pub(crate) const fn resolve_attention(self) -> bool {
        self.resolve_attention
    }

    pub(crate) const fn attention_status(self) -> AttentionItemStatus {
        self.attention_status
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
    let Some(attention) = current_plan_review(snapshot)? else {
        return Ok(None);
    };

    let context = decode_canonical_context(&attention.context)?;
    validate_context_encoding(&context)?;
    validate_context_payload(snapshot, &context)?;
    validate_current_authority(snapshot, attention, &context)?;
    let settlement = resolve_settlement(snapshot, attention, &context)?;

    Ok(Some(ValidatedSolutionReviewSet {
        delivery_id: context.delivery_id,
        delivery_spec_id: context.delivery_spec_id,
        delivery_spec_revision: context.delivery_spec_revision,
        planning_work_run_id: context.planning_work_run_id,
        planning_session_binding_id: context.planning_session_binding_id,
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
        work_item_proposals: context.work_item_proposals,
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
) -> Result<Option<&AttentionItem>, SolutionReviewError> {
    let candidates: Vec<_> = snapshot
        .attention_items
        .iter()
        .filter(|attention| {
            attention.delivery_id == snapshot.id
                && attention.delivery_spec_id == snapshot.spec.id
                && decode_canonical_context(&attention.context).is_ok_and(|context| {
                    context.attention_item_id == attention.id
                        && attention.work_run_id.as_ref() == Some(&context.planning_work_run_id)
                })
                && attention.item_type == AttentionItemType::DecisionRequired
                && attention.blocking
        })
        .collect();
    let Some(latest) = candidates
        .iter()
        .map(|attention| attention.created_at_millis)
        .max()
    else {
        return Ok(None);
    };
    let current: Vec<_> = candidates
        .into_iter()
        .filter(|attention| attention.created_at_millis == latest)
        .collect();
    let [attention] = current.as_slice() else {
        return Err(review_error(
            SolutionReviewErrorCode::AmbiguousCurrentReview,
            "current Delivery has more than one solution review at the latest timestamp",
        ));
    };
    Ok(Some(attention))
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
    validate_work_item_proposals(snapshot, &context.work_item_proposals)?;
    safe_time(context.prepared_at)?;
    Ok(())
}

fn validate_current_authority(
    snapshot: &crate::domain::DeliverySnapshot,
    attention: &AttentionItem,
    context: &SolutionReviewContextV1,
) -> Result<(), SolutionReviewError> {
    if context.delivery_id != snapshot.id
        || context.delivery_spec_id != snapshot.spec.id
        || context.delivery_spec_revision != snapshot.spec.revision
        || context.attention_item_id != attention.id
        || attention.work_run_id.as_ref() != Some(&context.planning_work_run_id)
        || attention.created_at_millis != context.prepared_at
    {
        return Err(stale_authority(
            "solution-review context does not match the current Delivery, Spec, review, or Attention",
        ));
    }

    let (_, binding_id) = current_planning_authority(snapshot, &context.planning_work_run_id)?;
    let planning_binding = snapshot
        .session_bindings
        .iter()
        .find(|binding| binding.id == binding_id)
        .ok_or_else(|| stale_authority("planning binding is missing"))?;
    if binding_id != context.planning_session_binding_id
        || context.prepared_at < planning_binding.bound_at_millis
    {
        return Err(stale_authority(
            "solution-review planning WorkRun or Attention authority is not current",
        ));
    }
    Ok(())
}

fn resolve_settlement(
    snapshot: &crate::domain::DeliverySnapshot,
    attention: &AttentionItem,
    context: &SolutionReviewContextV1,
) -> Result<Settlement, SolutionReviewError> {
    if attention.status == AttentionItemStatus::Open {
        if attention.resolution.is_some()
            || attention.resolved_by.is_some()
            || attention.resolved_at_millis.is_some()
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
        || decision.delivery_id != snapshot.id
        || decision.delivery_spec_id != snapshot.spec.id
        || decision.delivery_spec_revision != snapshot.spec.revision
        || decision.attention_item_id != attention.id
        || decision.review_set_sha256 != context.review_set_sha256
    {
        return Err(stale_authority(
            "solution-review decision does not match its current reviewer, time, or authority",
        ));
    }

    let (review_status, typed_decision, expected_attention) = match decision.action {
        DecisionActionWire::Approve => (
            ValidatedReviewStatus::Approved,
            ValidatedReviewDecision::Approve,
            AttentionItemStatus::Resolved,
        ),
        DecisionActionWire::RequestChanges => (
            ValidatedReviewStatus::ChangesRequested,
            ValidatedReviewDecision::RequestChanges,
            AttentionItemStatus::Dismissed,
        ),
        DecisionActionWire::Reject => (
            ValidatedReviewStatus::Rejected,
            ValidatedReviewDecision::Reject,
            AttentionItemStatus::Dismissed,
        ),
    };
    if attention.status != expected_attention
        || !settlement_status_is_current(decision.action, snapshot.status)
    {
        return Err(stale_authority(
            "solution-review decision does not match Attention and Delivery settlement",
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

// Called only after the persisted Attention, planning WorkRun, actor, time,
// decision, digest, Spec revision, and highest attempt have matched exactly.
const fn settlement_status_is_current(
    action: DecisionActionWire,
    status: crate::domain::DeliveryStatus,
) -> bool {
    match action {
        DecisionActionWire::Approve => matches!(
            status,
            crate::domain::DeliveryStatus::Ready
                | crate::domain::DeliveryStatus::Reworking
                | crate::domain::DeliveryStatus::NeedsAttention
                | crate::domain::DeliveryStatus::ReadyToDeliver
                | crate::domain::DeliveryStatus::Delivered
        ),
        DecisionActionWire::RequestChanges | DecisionActionWire::Reject => {
            matches!(status, crate::domain::DeliveryStatus::Clarifying)
        }
    }
}

/// Validates one pending plan-review decision before the Attention aggregate mutates.
pub(crate) fn validate_solution_review_settlement(
    delivery: &Delivery,
    attention_item_id: &AttentionItemId,
    actor: &str,
    resolution: &str,
    now_millis: u64,
) -> Result<ValidatedSolutionReviewSettlement, SolutionReviewError> {
    let review = resolve_current_solution_review(delivery)?
        .ok_or_else(|| stale_authority("plan-review settlement has no current review"))?;
    if review.review_status != ValidatedReviewStatus::Pending
        || &review.attention_item_id != attention_item_id
    {
        return Err(stale_authority(
            "plan-review settlement is not for the exact pending review",
        ));
    }

    let snapshot = delivery.snapshot();
    let attention = current_plan_review(snapshot)?
        .ok_or_else(|| stale_authority("plan-review settlement has no current authority"))?;
    portable_id(actor)?;
    safe_time(now_millis)?;
    if attention
        .assigned_to
        .as_deref()
        .is_some_and(|id| id != actor)
        || now_millis < attention.created_at_millis
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
            delivery_status: crate::domain::DeliveryStatus::Ready,
        },
        DecisionActionWire::RequestChanges | DecisionActionWire::Reject => {
            ValidatedSolutionReviewSettlement {
                resolve_attention: false,
                attention_status: AttentionItemStatus::Dismissed,
                delivery_status: crate::domain::DeliveryStatus::Clarifying,
            }
        }
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

fn validate_work_item_proposals(
    snapshot: &crate::domain::DeliverySnapshot,
    work_item_proposals: &[WorkItemProposal],
) -> Result<(), SolutionReviewError> {
    bounded_collection(work_item_proposals.len(), true)?;
    let current_criteria: HashSet<&str> = snapshot
        .spec
        .acceptance_criteria
        .iter()
        .map(|criterion| criterion.id.0.as_str())
        .collect();
    let mut task_ids = HashSet::new();
    let mut covered_criteria = HashSet::new();
    for proposal in work_item_proposals {
        portable_id(&proposal.id.0)?;
        safe_text(&proposal.title, MAX_TITLE_CODE_UNITS)?;
        safe_text(&proposal.goal, MAX_TEXT_CODE_UNITS)?;
        bounded_collection(proposal.criterion_ids.len(), true)?;
        bounded_collection(proposal.depends_on.len(), false)?;
        if !task_ids.insert(proposal.id.0.as_str()) {
            return Err(invalid_content(
                "solution-review WorkItem proposal identity is duplicated",
            ));
        }
        let mut proposal_criteria = HashSet::new();
        for criterion in &proposal.criterion_ids {
            portable_id(&criterion.0)?;
            if !current_criteria.contains(criterion.0.as_str())
                || !proposal_criteria.insert(criterion.0.as_str())
            {
                return Err(invalid_content(
                    "solution-review WorkItem proposal has a duplicate or foreign criterion",
                ));
            }
            covered_criteria.insert(criterion.0.as_str());
        }
        let mut dependencies = HashSet::new();
        for dependency in &proposal.depends_on {
            portable_id(&dependency.0)?;
            if dependency == &proposal.id || !dependencies.insert(dependency.0.as_str()) {
                return Err(invalid_content(
                    "solution-review WorkItem proposal dependency is self-referential or duplicated",
                ));
            }
        }
    }
    if covered_criteria != current_criteria {
        return Err(invalid_content(
            "solution-review WorkItem proposals do not cover every current criterion",
        ));
    }
    let proposals_by_id: HashMap<&str, &WorkItemProposal> = work_item_proposals
        .iter()
        .map(|proposal| (proposal.id.0.as_str(), proposal))
        .collect();
    for proposal in work_item_proposals {
        if proposal
            .depends_on
            .iter()
            .any(|dependency| !proposals_by_id.contains_key(dependency.0.as_str()))
        {
            return Err(invalid_content(
                "solution-review WorkItem proposal dependency is missing",
            ));
        }
    }
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for proposal in work_item_proposals {
        visit_work_item_proposal(proposal, &proposals_by_id, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit_work_item_proposal<'proposal>(
    proposal: &'proposal WorkItemProposal,
    proposals: &HashMap<&'proposal str, &'proposal WorkItemProposal>,
    visiting: &mut HashSet<&'proposal str>,
    visited: &mut HashSet<&'proposal str>,
) -> Result<(), SolutionReviewError> {
    let id = proposal.id.0.as_str();
    if visiting.contains(id) {
        return Err(invalid_content(
            "solution-review WorkItem proposal dependencies contain a cycle",
        ));
    }
    if visited.contains(id) {
        return Ok(());
    }
    visiting.insert(id);
    for dependency in &proposal.depends_on {
        visit_work_item_proposal(
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
        planning_work_run_id: &context.planning_work_run_id,
        planning_session_binding_id: &context.planning_session_binding_id,
        attention_item_id: &context.attention_item_id,
        solution: &context.solution,
        architecture_diagram: &context.architecture_diagram,
        process_diagram: &context.process_diagram,
        risks: &context.risks,
        unresolved_items: &context.unresolved_items,
        work_item_proposals: &context.work_item_proposals,
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

#[cfg(test)]
mod tests {
    use super::resolve_current_solution_review;
    use crate::domain::Delivery;
    use crate::store::{
        CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryQuery, DeliveryQueryPort,
        DeliveryStore, InMemoryDeliveryJournal,
    };
    use winwincode_domain::RequestId;

    #[test]
    fn current_workrun_solution_review_fixture_resolves() {
        let delivery = Delivery::decode_json(include_bytes!(
            "../../tests/fixtures/delivery-approved-solution-review.json"
        ))
        .expect("current Delivery fixture");
        let mut snapshot = delivery.into_snapshot();
        let attention = snapshot
            .attention_items
            .first_mut()
            .expect("solution-review Attention");
        attention.assigned_to = Some("usr_reviewer".into());
        attention.resolved_by = Some("usr_reviewer".into());
        snapshot.updated_at_millis += 1;
        let delivery = Delivery::try_from_snapshot(snapshot).expect("current Delivery");

        resolve_current_solution_review(&delivery)
            .expect("current solution-review authority")
            .expect("solution review");
        crate::projection::project_delivery_detail(crate::projection::ProjectionInput::new(
            &delivery,
        ))
        .expect("current solution-review projection");

        let journal = InMemoryDeliveryJournal::new();
        let store = DeliveryStore::borrowed(&journal);
        store
            .execute(DeliveryCommand::SeedForTest(CreateDelivery {
                request_id: RequestId("1".repeat(64)),
                request_digest: "1".repeat(64),
                snapshot: delivery.clone(),
            }))
            .expect("seed current Delivery");
        let loaded = store
            .query(DeliveryQuery::Get(delivery.id().clone()))
            .expect("reload current Delivery");
        crate::projection::project_delivery_detail(crate::projection::ProjectionInput::new(
            &loaded,
        ))
        .expect("reloaded solution-review projection");
    }
}
