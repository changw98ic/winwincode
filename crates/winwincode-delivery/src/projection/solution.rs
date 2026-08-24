// SPDX-License-Identifier: Apache-2.0

//! Approved plan-review projection.
//!
//! [`ApprovedSolutionReviewSet`] is opaque and has no public constructor or
//! deserializer. The legacy Attention protocol is not parsed at this public
//! seam. A future trusted plan-review adapter must validate that protocol and
//! create the sealed fact inside this crate; until then production projection
//! remains closed while unit tests exercise the full current-fact checks.
//!
//! ```compile_fail
//! use winwincode_delivery::projection::ApprovedSolutionReviewSet;
//!
//! let _caller_supplied: ApprovedSolutionReviewSet = serde_json::from_str("{}").unwrap();
//! ```

use std::collections::HashSet;

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{AttentionItemId, DeliveryId, StageRunId};

use crate::domain::{
    AttentionItemStatus, AttentionItemType, Delivery, DeliverySpecId, DeliveryStage,
    SessionBindingId, StageRunActorType, StageRunStatus,
};

use super::{ProjectionError, ProjectionErrorCode};

const MAX_PUBLIC_TEXT_LENGTH: usize = 65_536;
const MAX_PUBLIC_COLLECTION_LENGTH: usize = 200;
const MAX_REPOSITORY_PATH_LENGTH: usize = 4_096;
const PLATFORM_NODE_IDS: [&str; 4] = [
    "platform:dsh",
    "platform:strongflow",
    "platform:codex-core",
    "platform:repository",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HumanDecisionProjection {
    #[serde(rename = "approve")]
    Approve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SolutionComponentKind {
    #[serde(rename = "component")]
    Component,
    #[serde(rename = "external")]
    External,
    #[serde(rename = "data-store")]
    DataStore,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SolutionComponentProjection {
    id: String,
    label: String,
    responsibility: String,
    kind: SolutionComponentKind,
    trust_boundary: Option<String>,
    unresolved: bool,
    repository_path_prefixes: Vec<String>,
}

impl SolutionComponentProjection {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn responsibility(&self) -> &str {
        &self.responsibility
    }

    pub const fn kind(&self) -> SolutionComponentKind {
        self.kind
    }

    pub fn trust_boundary(&self) -> Option<&str> {
        self.trust_boundary.as_deref()
    }

    pub const fn unresolved(&self) -> bool {
        self.unresolved
    }

    pub fn repository_path_prefixes(&self) -> &[String] {
        &self.repository_path_prefixes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SolutionConnectionProjection {
    id: String,
    from: String,
    to: String,
    label: String,
}

impl SolutionConnectionProjection {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn from(&self) -> &str {
        &self.from
    }

    pub fn to(&self) -> &str {
        &self.to
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DiagramKind {
    #[serde(rename = "system-architecture")]
    SystemArchitecture,
    #[serde(rename = "process-flow")]
    ProcessFlow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DiagramNodeKind {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagramNodeProjection {
    id: String,
    label: String,
    description: String,
    kind: DiagramNodeKind,
    trust_boundary: Option<String>,
    unresolved: bool,
}

impl DiagramNodeProjection {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub const fn kind(&self) -> DiagramNodeKind {
        self.kind
    }

    pub fn trust_boundary(&self) -> Option<&str> {
        self.trust_boundary.as_deref()
    }

    pub const fn unresolved(&self) -> bool {
        self.unresolved
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagramEdgeProjection {
    id: String,
    from: String,
    to: String,
    label: String,
}

impl DiagramEdgeProjection {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn from(&self) -> &str {
        &self.from
    }

    pub fn to(&self) -> &str {
        &self.to
    }

    pub fn label(&self) -> &str {
        &self.label
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagramProjection {
    id: String,
    kind: DiagramKind,
    title: String,
    nodes: Vec<DiagramNodeProjection>,
    edges: Vec<DiagramEdgeProjection>,
}

impl DiagramProjection {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn kind(&self) -> DiagramKind {
        self.kind
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn nodes(&self) -> &[DiagramNodeProjection] {
        &self.nodes
    }

    pub fn edges(&self) -> &[DiagramEdgeProjection] {
        &self.edges
    }
}

/// One approved, current and safe solution view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SolutionProjection {
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
    components: Vec<SolutionComponentProjection>,
    connections: Vec<SolutionConnectionProjection>,
    architecture_diagram: DiagramProjection,
    process_diagram: DiagramProjection,
    risks: Vec<String>,
    unresolved_items: Vec<String>,
    reviewer_id: String,
    human_decision: HumanDecisionProjection,
    reviewed_at: u64,
}

impl SolutionProjection {
    pub fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    pub fn delivery_spec_id(&self) -> &DeliverySpecId {
        &self.delivery_spec_id
    }

    pub const fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }

    pub fn planning_stage_run_id(&self) -> &StageRunId {
        &self.planning_stage_run_id
    }

    pub fn planning_session_binding_id(&self) -> &SessionBindingId {
        &self.planning_session_binding_id
    }

    pub fn review_stage_run_id(&self) -> &StageRunId {
        &self.review_stage_run_id
    }

    pub fn attention_item_id(&self) -> &AttentionItemId {
        &self.attention_item_id
    }

    pub fn review_set_sha256(&self) -> &str {
        &self.review_set_sha256
    }

    pub fn solution_id(&self) -> &str {
        &self.solution_id
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn approach(&self) -> &[String] {
        &self.approach
    }

    pub fn components(&self) -> &[SolutionComponentProjection] {
        &self.components
    }

    pub fn connections(&self) -> &[SolutionConnectionProjection] {
        &self.connections
    }

    pub const fn architecture_diagram(&self) -> &DiagramProjection {
        &self.architecture_diagram
    }

    pub const fn process_diagram(&self) -> &DiagramProjection {
        &self.process_diagram
    }

    pub fn risks(&self) -> &[String] {
        &self.risks
    }

    pub fn unresolved_items(&self) -> &[String] {
        &self.unresolved_items
    }

    pub fn reviewer_id(&self) -> &str {
        &self.reviewer_id
    }

    pub const fn human_decision(&self) -> HumanDecisionProjection {
        self.human_decision
    }

    pub const fn reviewed_at(&self) -> u64 {
        self.reviewed_at
    }
}

/// Sealed result of the future trusted plan-review protocol adapter.
///
/// It deliberately excludes a review `SessionBinding`: a human plan-review
/// `StageRun` is not an `ExecutionJob`. Human authority is bound through the exact
/// review `StageRun`, `AttentionItem`, authenticated `resolved_by`, decision,
/// review digest, and review time.
#[derive(Clone, PartialEq, Eq)]
pub struct ApprovedSolutionReviewSet {
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
    components: Vec<SolutionComponentProjection>,
    connections: Vec<SolutionConnectionProjection>,
    architecture_diagram: DiagramProjection,
    process_diagram: DiagramProjection,
    risks: Vec<String>,
    unresolved_items: Vec<String>,
    reviewer_id: String,
    prepared_at: u64,
    reviewed_at: u64,
    validation_seal: [u8; 32],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewSetSealIdentity<'fact> {
    delivery_id: &'fact DeliveryId,
    delivery_spec_id: &'fact DeliverySpecId,
    delivery_spec_revision: u64,
    planning_stage_run_id: &'fact StageRunId,
    planning_session_binding_id: &'fact SessionBindingId,
    review_stage_run_id: &'fact StageRunId,
    attention_item_id: &'fact AttentionItemId,
    review_set_sha256: &'fact str,
    solution_id: &'fact str,
    summary: &'fact str,
    approach: &'fact [String],
    components: &'fact [SolutionComponentProjection],
    connections: &'fact [SolutionConnectionProjection],
    architecture_diagram: &'fact DiagramProjection,
    process_diagram: &'fact DiagramProjection,
    risks: &'fact [String],
    unresolved_items: &'fact [String],
    reviewer_id: &'fact str,
    prepared_at: u64,
    reviewed_at: u64,
}

pub(super) fn project_current_solution(
    delivery: &Delivery,
    approved: &ApprovedSolutionReviewSet,
) -> Result<SolutionProjection, ProjectionError> {
    validate_sealed_fact(approved)?;
    validate_current_authority(delivery, approved)?;
    validate_safe_payload(approved)?;

    let mut components = approved.components.clone();
    for component in &mut components {
        component.repository_path_prefixes.sort();
    }
    components.sort_by(|left, right| left.id.cmp(&right.id));
    let mut connections = approved.connections.clone();
    connections.sort_by(|left, right| left.id.cmp(&right.id));
    let architecture_diagram = ordered_diagram(approved.architecture_diagram.clone());
    let process_diagram = ordered_diagram(approved.process_diagram.clone());
    let mut risks = approved.risks.clone();
    risks.sort();
    let mut unresolved_items = approved.unresolved_items.clone();
    unresolved_items.sort();

    Ok(SolutionProjection {
        delivery_id: approved.delivery_id.clone(),
        delivery_spec_id: approved.delivery_spec_id.clone(),
        delivery_spec_revision: approved.delivery_spec_revision,
        planning_stage_run_id: approved.planning_stage_run_id.clone(),
        planning_session_binding_id: approved.planning_session_binding_id.clone(),
        review_stage_run_id: approved.review_stage_run_id.clone(),
        attention_item_id: approved.attention_item_id.clone(),
        review_set_sha256: approved.review_set_sha256.clone(),
        solution_id: approved.solution_id.clone(),
        summary: approved.summary.clone(),
        approach: approved.approach.clone(),
        components,
        connections,
        architecture_diagram,
        process_diagram,
        risks,
        unresolved_items,
        reviewer_id: approved.reviewer_id.clone(),
        human_decision: HumanDecisionProjection::Approve,
        reviewed_at: approved.reviewed_at,
    })
}

fn validate_current_authority(
    delivery: &Delivery,
    approved: &ApprovedSolutionReviewSet,
) -> Result<(), ProjectionError> {
    let snapshot = delivery.snapshot();
    if approved.delivery_id != snapshot.id
        || approved.delivery_spec_id != snapshot.spec.id
        || approved.delivery_spec_revision != snapshot.spec.revision
    {
        return Err(stale_solution(
            "approved solution does not match the current DeliverySpec",
        ));
    }
    let planning = snapshot
        .stage_runs
        .iter()
        .find(|run| run.id == approved.planning_stage_run_id)
        .ok_or_else(|| stale_solution("approved solution planning StageRun is missing"))?;
    let review = snapshot
        .stage_runs
        .iter()
        .find(|run| run.id == approved.review_stage_run_id)
        .ok_or_else(|| stale_solution("approved solution review StageRun is missing"))?;
    let attention = snapshot
        .attention_items
        .iter()
        .find(|item| item.id == approved.attention_item_id)
        .ok_or_else(|| stale_solution("approved solution AttentionItem is missing"))?;
    let planning_bindings: Vec<_> = snapshot
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == planning.id)
        .collect();
    let [planning_binding] = planning_bindings.as_slice() else {
        return Err(stale_solution(
            "approved solution requires one exact planning SessionBinding",
        ));
    };
    if planning_binding.id != approved.planning_session_binding_id
        || planning_binding.worker_session_id.is_none()
        || planning_binding.codex_thread_id.is_none()
        || planning.stage != DeliveryStage::Planning
        || planning.actor_type != StageRunActorType::Codex
        || planning.role != "planner"
        || planning.status != StageRunStatus::Succeeded
        || planning.finished_at_millis.is_none()
        || review.stage != DeliveryStage::PlanReview
        || review.actor_type != StageRunActorType::Human
        || review.role != "reviewer"
        || review.status != StageRunStatus::Succeeded
        || review.finished_at_millis.is_none()
        || snapshot
            .session_bindings
            .iter()
            .any(|binding| binding.stage_run_id == review.id)
        || attention.delivery_id != snapshot.id
        || attention.delivery_spec_id != snapshot.spec.id
        || attention.stage_run_id.as_ref() != Some(&review.id)
        || attention.item_type != AttentionItemType::DecisionRequired
        || !attention.blocking
        || attention.status != AttentionItemStatus::Resolved
        || attention.resolved_by.as_deref() != Some(approved.reviewer_id.as_str())
        || attention.resolved_at_millis != Some(approved.reviewed_at)
        || attention.created_at_millis != approved.prepared_at
    {
        return Err(stale_solution(
            "approved solution does not match its planning, human review, or Attention authority",
        ));
    }
    let planning_finished = planning.finished_at_millis.expect("checked finished time");
    let review_finished = review.finished_at_millis.expect("checked finished time");
    if approved.prepared_at < planning.started_at_millis
        || approved.prepared_at < planning_binding.bound_at_millis
        || approved.prepared_at > planning_finished
        || planning_finished > review.started_at_millis
        || approved.prepared_at > review.started_at_millis
        || approved.reviewed_at != review_finished
    {
        return Err(stale_solution(
            "approved solution review times do not follow the current planning and review runs",
        ));
    }
    Ok(())
}

fn validate_sealed_fact(approved: &ApprovedSolutionReviewSet) -> Result<(), ProjectionError> {
    let expected = seal_review_set(approved)?;
    if expected != approved.validation_seal || !lowercase_sha256(&approved.review_set_sha256) {
        return Err(stale_solution(
            "approved solution review-set digest or validation seal changed",
        ));
    }
    Ok(())
}

fn validate_safe_payload(approved: &ApprovedSolutionReviewSet) -> Result<(), ProjectionError> {
    portable_id(&approved.solution_id)?;
    public_text(&approved.summary)?;
    portable_id(&approved.reviewer_id)?;
    validate_text_list(&approved.approach, true)?;
    validate_text_list(&approved.risks, false)?;
    validate_text_list(&approved.unresolved_items, false)?;
    if approved.components.is_empty()
        || approved.components.len() > MAX_PUBLIC_COLLECTION_LENGTH
        || approved.connections.len() > MAX_PUBLIC_COLLECTION_LENGTH
    {
        return Err(stale_solution(
            "approved solution collection size is invalid",
        ));
    }
    let mut component_ids = HashSet::new();
    for component in &approved.components {
        portable_id(&component.id)?;
        public_text(&component.label)?;
        public_text(&component.responsibility)?;
        if let Some(boundary) = &component.trust_boundary {
            public_text(boundary)?;
        }
        if PLATFORM_NODE_IDS.contains(&component.id.as_str())
            || !component_ids.insert(component.id.as_str())
            || component.repository_path_prefixes.len() > MAX_PUBLIC_COLLECTION_LENGTH
        {
            return Err(stale_solution(
                "approved solution component identity is invalid",
            ));
        }
        let mut prefixes = HashSet::new();
        for prefix in &component.repository_path_prefixes {
            repository_path_prefix(prefix)?;
            if !prefixes.insert(prefix) {
                return Err(stale_solution(
                    "approved solution repeats a repository path prefix",
                ));
            }
        }
    }
    let mut connection_ids = HashSet::new();
    let allowed_endpoints: HashSet<_> = PLATFORM_NODE_IDS
        .iter()
        .copied()
        .chain(component_ids.iter().copied())
        .collect();
    for connection in &approved.connections {
        portable_id(&connection.id)?;
        portable_id(&connection.from)?;
        portable_id(&connection.to)?;
        public_text(&connection.label)?;
        if !connection_ids.insert(connection.id.as_str())
            || connection.from == connection.to
            || !allowed_endpoints.contains(connection.from.as_str())
            || !allowed_endpoints.contains(connection.to.as_str())
        {
            return Err(stale_solution("approved solution connection is invalid"));
        }
    }
    validate_diagram(
        &approved.architecture_diagram,
        DiagramKind::SystemArchitecture,
    )?;
    validate_diagram(&approved.process_diagram, DiagramKind::ProcessFlow)
}

fn validate_diagram(
    diagram: &DiagramProjection,
    expected_kind: DiagramKind,
) -> Result<(), ProjectionError> {
    portable_id(&diagram.id)?;
    public_text(&diagram.title)?;
    if diagram.kind != expected_kind
        || diagram.nodes.is_empty()
        || diagram.nodes.len() > MAX_PUBLIC_COLLECTION_LENGTH
        || diagram.edges.len() > MAX_PUBLIC_COLLECTION_LENGTH
    {
        return Err(stale_solution("approved solution diagram shape is invalid"));
    }
    let mut node_ids = HashSet::new();
    for node in &diagram.nodes {
        portable_id(&node.id)?;
        public_text(&node.label)?;
        public_text(&node.description)?;
        if let Some(boundary) = &node.trust_boundary {
            public_text(boundary)?;
        }
        if !node_ids.insert(node.id.as_str()) {
            return Err(stale_solution("approved solution diagram repeats a node"));
        }
    }
    let mut edge_ids = HashSet::new();
    for edge in &diagram.edges {
        portable_id(&edge.id)?;
        portable_id(&edge.from)?;
        portable_id(&edge.to)?;
        public_text(&edge.label)?;
        if !edge_ids.insert(edge.id.as_str())
            || edge.from == edge.to
            || !node_ids.contains(edge.from.as_str())
            || !node_ids.contains(edge.to.as_str())
        {
            return Err(stale_solution("approved solution diagram edge is invalid"));
        }
    }
    Ok(())
}

fn validate_text_list(values: &[String], required: bool) -> Result<(), ProjectionError> {
    if (required && values.is_empty()) || values.len() > MAX_PUBLIC_COLLECTION_LENGTH {
        return Err(stale_solution(
            "approved solution text collection is invalid",
        ));
    }
    let mut unique = HashSet::new();
    for value in values {
        public_text(value)?;
        if !unique.insert(value) {
            return Err(stale_solution("approved solution repeats a text item"));
        }
    }
    Ok(())
}

fn public_text(value: &str) -> Result<(), ProjectionError> {
    let has_forbidden_control = value
        .chars()
        .any(|character| matches!(u32::from(character), 0..=8 | 11..=12 | 14..=31 | 127));
    if value.trim().is_empty()
        || value.encode_utf16().count() > MAX_PUBLIC_TEXT_LENGTH
        || has_forbidden_control
    {
        return Err(stale_solution("approved solution contains unsafe text"));
    }
    Ok(())
}

fn portable_id(value: &str) -> Result<(), ProjectionError> {
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
        Err(stale_solution(
            "approved solution contains a non-portable identity",
        ))
    }
}

fn repository_path_prefix(value: &str) -> Result<(), ProjectionError> {
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
        Err(stale_solution(
            "approved solution contains an unsafe repository path prefix",
        ))
    }
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn ordered_diagram(mut diagram: DiagramProjection) -> DiagramProjection {
    diagram.nodes.sort_by(|left, right| left.id.cmp(&right.id));
    diagram.edges.sort_by(|left, right| left.id.cmp(&right.id));
    diagram
}

fn seal_review_set(approved: &ApprovedSolutionReviewSet) -> Result<[u8; 32], ProjectionError> {
    let identity = ReviewSetSealIdentity {
        delivery_id: &approved.delivery_id,
        delivery_spec_id: &approved.delivery_spec_id,
        delivery_spec_revision: approved.delivery_spec_revision,
        planning_stage_run_id: &approved.planning_stage_run_id,
        planning_session_binding_id: &approved.planning_session_binding_id,
        review_stage_run_id: &approved.review_stage_run_id,
        attention_item_id: &approved.attention_item_id,
        review_set_sha256: &approved.review_set_sha256,
        solution_id: &approved.solution_id,
        summary: &approved.summary,
        approach: &approved.approach,
        components: &approved.components,
        connections: &approved.connections,
        architecture_diagram: &approved.architecture_diagram,
        process_diagram: &approved.process_diagram,
        risks: &approved.risks,
        unresolved_items: &approved.unresolved_items,
        reviewer_id: &approved.reviewer_id,
        prepared_at: approved.prepared_at,
        reviewed_at: approved.reviewed_at,
    };
    let encoded = serde_json::to_vec(&identity)
        .map_err(|_| stale_solution("approved solution seal cannot be encoded"))?;
    Ok(Sha256::digest(encoded).into())
}

fn stale_solution(message: &str) -> ProjectionError {
    ProjectionError::new(ProjectionErrorCode::StaleApprovedSolution, message)
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        AttentionItemId, CodexThreadId, ExecutionJobId, ProductSessionId, StageRunId,
        WorkerSessionId,
    };

    use super::*;
    use crate::domain::{
        AttentionItem, AttentionItemStatus, AttentionItemType, DeliveryStatus, SessionBinding,
        StageRun, test_fixture,
    };
    use crate::projection::{ProjectionInput, project_delivery_detail};

    fn reviewed_delivery() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Executing;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        let planning = &mut snapshot.stage_runs[0];
        planning.id = StageRunId("stage-planning-1".into());
        planning.delivery_task_id = None;
        planning.stage = DeliveryStage::Planning;
        planning.actor_type = StageRunActorType::Codex;
        planning.role = "planner".into();
        planning.status = StageRunStatus::Succeeded;
        planning.started_at_millis = 1_800_000_000_010;
        planning.finished_at_millis = Some(1_800_000_000_020);
        let binding = &mut snapshot.session_bindings[0];
        binding.id = SessionBindingId("binding-planner-1".into());
        binding.delivery_task_id = None;
        binding.stage_run_id = planning.id.clone();
        binding.product_session_id = ProductSessionId("product-planner".into());
        binding.execution_job_id = ExecutionJobId("job-planner".into());
        binding.worker_session_id = Some(WorkerSessionId("worker-planner".into()));
        binding.codex_thread_id = Some(CodexThreadId("thread-planner".into()));
        binding.bound_at_millis = 1_800_000_000_011;
        snapshot.stage_runs.push(StageRun {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: StageRunId("stage-plan-review-1".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: None,
            stage: DeliveryStage::PlanReview,
            actor_type: StageRunActorType::Human,
            role: "reviewer".into(),
            status: StageRunStatus::Succeeded,
            attempt: 1,
            started_at_millis: 1_800_000_000_021,
            finished_at_millis: Some(1_800_000_000_030),
        });
        snapshot.attention_items.push(AttentionItem {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: AttentionItemId("attention-plan-review-1".into()),
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            stage_run_id: Some(StageRunId("stage-plan-review-1".into())),
            item_type: AttentionItemType::DecisionRequired,
            title: "Review delivery solution".into(),
            context:
                r#"{"apiKey":"must-not-leak","toolPayload":"hidden","rawRuntimeLog":"hidden"}"#
                    .into(),
            options: vec![],
            assigned_to: Some("alice".into()),
            blocking: true,
            status: AttentionItemStatus::Resolved,
            resolution: Some(
                r#"{"authorization":"must-not-leak","stdout":"hidden","stderr":"hidden"}"#.into(),
            ),
            resolved_by: Some("alice".into()),
            created_at_millis: 1_800_000_000_019,
            resolved_at_millis: Some(1_800_000_000_030),
        });
        snapshot.updated_at_millis = 1_800_000_000_031;
        Delivery::try_from_snapshot(snapshot).expect("approved plan-review Delivery")
    }

    fn diagram(id: &str, kind: DiagramKind) -> DiagramProjection {
        DiagramProjection {
            id: id.into(),
            kind,
            title: format!("{id} title"),
            nodes: vec![
                DiagramNodeProjection {
                    id: "node:b".into(),
                    label: "Worker".into(),
                    description: "Executes approved work.".into(),
                    kind: DiagramNodeKind::Execution,
                    trust_boundary: Some("worker".into()),
                    unresolved: false,
                },
                DiagramNodeProjection {
                    id: "node:a".into(),
                    label: "Control Plane".into(),
                    description: "Owns delivery decisions.".into(),
                    kind: DiagramNodeKind::DeliveryControl,
                    trust_boundary: Some("control-plane".into()),
                    unresolved: false,
                },
            ],
            edges: vec![DiagramEdgeProjection {
                id: "edge:a-b".into(),
                from: "node:a".into(),
                to: "node:b".into(),
                label: "dispatches".into(),
            }],
        }
    }

    fn approved_review_set(delivery: &Delivery) -> ApprovedSolutionReviewSet {
        let mut approved = ApprovedSolutionReviewSet {
            delivery_id: delivery.id().clone(),
            delivery_spec_id: delivery.snapshot().spec.id.clone(),
            delivery_spec_revision: delivery.snapshot().spec.revision,
            planning_stage_run_id: StageRunId("stage-planning-1".into()),
            planning_session_binding_id: SessionBindingId("binding-planner-1".into()),
            review_stage_run_id: StageRunId("stage-plan-review-1".into()),
            attention_item_id: AttentionItemId("attention-plan-review-1".into()),
            review_set_sha256: "a".repeat(64),
            solution_id: "solution:invitation".into(),
            summary: "Implement the invitation flow through one controlled worker.".into(),
            approach: vec!["Define API".into(), "Verify acceptance".into()],
            components: vec![
                SolutionComponentProjection {
                    id: "component:web".into(),
                    label: "Web".into(),
                    responsibility: "Renders invitation state.".into(),
                    kind: SolutionComponentKind::Component,
                    trust_boundary: Some("browser".into()),
                    unresolved: false,
                    repository_path_prefixes: vec!["apps/web".into()],
                },
                SolutionComponentProjection {
                    id: "component:api".into(),
                    label: "API".into(),
                    responsibility: "Accepts invitations once.".into(),
                    kind: SolutionComponentKind::Component,
                    trust_boundary: Some("control-plane".into()),
                    unresolved: false,
                    repository_path_prefixes: vec!["crates/api".into()],
                },
            ],
            connections: vec![SolutionConnectionProjection {
                id: "connection:web-api".into(),
                from: "component:web".into(),
                to: "component:api".into(),
                label: "HTTP".into(),
            }],
            architecture_diagram: diagram("diagram:architecture", DiagramKind::SystemArchitecture),
            process_diagram: diagram("diagram:process", DiagramKind::ProcessFlow),
            risks: vec!["Invitation replay".into()],
            unresolved_items: vec![],
            reviewer_id: "alice".into(),
            prepared_at: 1_800_000_000_019,
            reviewed_at: 1_800_000_000_030,
            validation_seal: [0; 32],
        };
        approved.validation_seal = seal_review_set(&approved).expect("review-set seal");
        approved
    }

    #[test]
    fn solution_projection_requires_current_approved_review_set() {
        let delivery = reviewed_delivery();
        let approved = approved_review_set(&delivery);

        let projection = project_delivery_detail(
            ProjectionInput::new(&delivery).with_approved_solution(&approved),
        )
        .expect("approved solution projection");
        let solution = projection.solution().expect("approved solution");
        assert_eq!(solution.review_set_sha256(), "a".repeat(64));
        assert_eq!(solution.reviewer_id(), "alice");
        assert_eq!(solution.human_decision(), HumanDecisionProjection::Approve);
        assert_eq!(solution.components()[0].id(), "component:api");
        let wire = serde_json::to_value(solution).expect("solution wire projection");
        assert_eq!(wire["reviewerId"], "alice");
        assert_eq!(wire["humanDecision"], "approve");
        assert!(wire.get("reviewer_id").is_none());

        let mut revised = delivery.into_snapshot();
        revised.spec.revision += 1;
        revised.revision += 1;
        let revised = Delivery::try_from_snapshot(revised).expect("revised Delivery");
        assert_eq!(
            project_delivery_detail(
                ProjectionInput::new(&revised).with_approved_solution(&approved)
            )
            .expect_err("old review set")
            .code(),
            ProjectionErrorCode::StaleApprovedSolution
        );
    }

    #[test]
    fn human_review_cannot_forge_execution_session_binding() {
        let delivery = reviewed_delivery();
        let mut snapshot = delivery.into_snapshot();
        snapshot.session_bindings.push(SessionBinding {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId("binding-forged-human-review".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: None,
            stage_run_id: StageRunId("stage-plan-review-1".into()),
            product_session_id: ProductSessionId("product-forged-review".into()),
            execution_job_id: ExecutionJobId("job-forged-review".into()),
            worker_session_id: Some(WorkerSessionId("worker-forged-review".into())),
            codex_thread_id: Some(CodexThreadId("thread-forged-review".into())),
            bound_at_millis: 1_800_000_000_022,
        });

        assert!(Delivery::try_from_snapshot(snapshot).is_err());
    }

    #[test]
    fn solution_projection_rejects_foreign_binding_attention_and_reviewer_authority() {
        let delivery = reviewed_delivery();

        let mut foreign_binding = approved_review_set(&delivery);
        foreign_binding.planning_session_binding_id = SessionBindingId("binding-foreign".into());
        foreign_binding.validation_seal =
            seal_review_set(&foreign_binding).expect("foreign binding seal");
        assert_eq!(
            project_delivery_detail(
                ProjectionInput::new(&delivery).with_approved_solution(&foreign_binding)
            )
            .expect_err("foreign planning binding")
            .code(),
            ProjectionErrorCode::StaleApprovedSolution
        );

        let mut foreign_attention = approved_review_set(&delivery);
        foreign_attention.attention_item_id = AttentionItemId("attention-foreign".into());
        foreign_attention.validation_seal =
            seal_review_set(&foreign_attention).expect("foreign Attention seal");
        assert_eq!(
            project_delivery_detail(
                ProjectionInput::new(&delivery).with_approved_solution(&foreign_attention)
            )
            .expect_err("foreign Attention")
            .code(),
            ProjectionErrorCode::StaleApprovedSolution
        );

        let mut foreign_reviewer = approved_review_set(&delivery);
        foreign_reviewer.reviewer_id = "mallory".into();
        foreign_reviewer.validation_seal =
            seal_review_set(&foreign_reviewer).expect("foreign reviewer seal");
        assert_eq!(
            project_delivery_detail(
                ProjectionInput::new(&delivery).with_approved_solution(&foreign_reviewer)
            )
            .expect_err("foreign authenticated reviewer")
            .code(),
            ProjectionErrorCode::StaleApprovedSolution
        );
    }

    #[test]
    fn solution_projection_rejects_non_portable_reviewer_id() {
        let delivery = reviewed_delivery();
        let mut snapshot = delivery.into_snapshot();
        snapshot.attention_items[0].resolved_by = Some("reviewer with spaces".into());
        let delivery = Delivery::try_from_snapshot(snapshot).expect("reviewer authority fixture");
        let mut approved = approved_review_set(&delivery);
        approved.reviewer_id = "reviewer with spaces".into();
        approved.validation_seal = seal_review_set(&approved).expect("non-portable reviewer seal");

        assert_eq!(
            project_delivery_detail(
                ProjectionInput::new(&delivery).with_approved_solution(&approved)
            )
            .expect_err("non-portable reviewer identity")
            .code(),
            ProjectionErrorCode::StaleApprovedSolution
        );
    }

    #[test]
    fn solution_projection_rejects_changed_review_digest_or_seal() {
        let delivery = reviewed_delivery();
        let mut changed_digest = approved_review_set(&delivery);
        changed_digest.review_set_sha256 = "b".repeat(64);
        assert_eq!(
            project_delivery_detail(
                ProjectionInput::new(&delivery).with_approved_solution(&changed_digest)
            )
            .expect_err("changed digest without a matching seal")
            .code(),
            ProjectionErrorCode::StaleApprovedSolution
        );

        let mut invalid_digest = approved_review_set(&delivery);
        invalid_digest.review_set_sha256 = "A".repeat(64);
        invalid_digest.validation_seal =
            seal_review_set(&invalid_digest).expect("invalid digest seal");
        assert_eq!(
            project_delivery_detail(
                ProjectionInput::new(&delivery).with_approved_solution(&invalid_digest)
            )
            .expect_err("non-canonical review digest")
            .code(),
            ProjectionErrorCode::StaleApprovedSolution
        );
    }

    #[test]
    fn solution_projection_excludes_raw_attention_secret_tool_and_log_content() {
        let delivery = reviewed_delivery();
        let raw_context = &delivery.snapshot().attention_items[0].context;
        let raw_resolution = delivery.snapshot().attention_items[0]
            .resolution
            .as_deref()
            .expect("raw resolution fixture");
        assert!(raw_context.contains("apiKey"));
        assert!(raw_context.contains("toolPayload"));
        assert!(raw_context.contains("rawRuntimeLog"));
        assert!(raw_resolution.contains("authorization"));
        assert!(raw_resolution.contains("stdout"));
        let secret_marker = "must-not-leak";
        assert!(raw_context.contains(secret_marker));
        let approved = approved_review_set(&delivery);
        let projection = project_delivery_detail(
            ProjectionInput::new(&delivery).with_approved_solution(&approved),
        )
        .expect("safe solution projection");
        let encoded = String::from_utf8(projection.encode_json().expect("projection JSON"))
            .expect("UTF-8 JSON");

        for forbidden in [
            secret_marker,
            "apiKey",
            "authorization",
            "toolPayload",
            "rawRuntimeLog",
            "stdout",
            "stderr",
        ] {
            assert!(!encoded.contains(forbidden), "leaked {forbidden}");
        }
    }
}
