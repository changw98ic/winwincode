// SPDX-License-Identifier: Apache-2.0

//! Safe public projection of one validated solution review.

use serde::Serialize;
use winwincode_domain::{AttentionItemId, DeliveryId, DeliveryTaskId, StageRunId};

use crate::{
    application::solution_review::{
        DeliveryTaskProposal, ValidatedDiagram, ValidatedDiagramEdge, ValidatedDiagramKind,
        ValidatedDiagramNode, ValidatedDiagramNodeKind, ValidatedReviewDecision,
        ValidatedReviewStatus, ValidatedSolutionComponent, ValidatedSolutionComponentKind,
        ValidatedSolutionConnection, ValidatedSolutionReviewSet,
    },
    domain::{AcceptanceCriterionId, DeliverySpecId, SessionBindingId},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SolutionReviewStatusProjection {
    #[serde(rename = "pending")]
    Pending,
    #[serde(rename = "approved")]
    Approved,
    #[serde(rename = "changes_requested")]
    ChangesRequested,
    #[serde(rename = "rejected")]
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SolutionReviewDecisionProjection {
    #[serde(rename = "approve")]
    Approve,
    #[serde(rename = "request_changes")]
    RequestChanges,
    #[serde(rename = "reject")]
    Reject,
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
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn responsibility(&self) -> &str {
        &self.responsibility
    }

    #[must_use]
    pub const fn kind(&self) -> SolutionComponentKind {
        self.kind
    }

    #[must_use]
    pub fn trust_boundary(&self) -> Option<&str> {
        self.trust_boundary.as_deref()
    }

    #[must_use]
    pub const fn unresolved(&self) -> bool {
        self.unresolved
    }

    #[must_use]
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
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn from(&self) -> &str {
        &self.from
    }

    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }

    #[must_use]
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
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub const fn kind(&self) -> DiagramNodeKind {
        self.kind
    }

    #[must_use]
    pub fn trust_boundary(&self) -> Option<&str> {
        self.trust_boundary.as_deref()
    }

    #[must_use]
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
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn from(&self) -> &str {
        &self.from
    }

    #[must_use]
    pub fn to(&self) -> &str {
        &self.to
    }

    #[must_use]
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
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn kind(&self) -> DiagramKind {
        self.kind
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn nodes(&self) -> &[DiagramNodeProjection] {
        &self.nodes
    }

    #[must_use]
    pub fn edges(&self) -> &[DiagramEdgeProjection] {
        &self.edges
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryTaskProposalProjection {
    id: DeliveryTaskId,
    title: String,
    goal: String,
    acceptance_criterion_ids: Vec<AcceptanceCriterionId>,
    blocked_by_task_ids: Vec<DeliveryTaskId>,
}

impl DeliveryTaskProposalProjection {
    #[must_use]
    pub fn id(&self) -> &DeliveryTaskId {
        &self.id
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn goal(&self) -> &str {
        &self.goal
    }

    #[must_use]
    pub fn acceptance_criterion_ids(&self) -> &[AcceptanceCriterionId] {
        &self.acceptance_criterion_ids
    }

    #[must_use]
    pub fn blocked_by_task_ids(&self) -> &[DeliveryTaskId] {
        &self.blocked_by_task_ids
    }
}

/// One current pending or settled solution-review view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SolutionReviewProjection {
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
    task_proposals: Vec<DeliveryTaskProposalProjection>,
    review_status: SolutionReviewStatusProjection,
    decision: Option<SolutionReviewDecisionProjection>,
    comments: Option<String>,
    requested_changes: Option<Vec<String>>,
    reviewer_id: Option<String>,
    reviewed_at: Option<u64>,
}

impl SolutionReviewProjection {
    #[must_use]
    pub fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }
    #[must_use]
    pub fn delivery_spec_id(&self) -> &DeliverySpecId {
        &self.delivery_spec_id
    }
    #[must_use]
    pub const fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }
    #[must_use]
    pub fn planning_stage_run_id(&self) -> &StageRunId {
        &self.planning_stage_run_id
    }
    #[must_use]
    pub fn planning_session_binding_id(&self) -> &SessionBindingId {
        &self.planning_session_binding_id
    }
    #[must_use]
    pub fn review_stage_run_id(&self) -> &StageRunId {
        &self.review_stage_run_id
    }
    #[must_use]
    pub fn attention_item_id(&self) -> &AttentionItemId {
        &self.attention_item_id
    }
    #[must_use]
    pub fn review_set_sha256(&self) -> &str {
        &self.review_set_sha256
    }
    #[must_use]
    pub fn solution_id(&self) -> &str {
        &self.solution_id
    }
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }
    #[must_use]
    pub fn approach(&self) -> &[String] {
        &self.approach
    }
    #[must_use]
    pub fn components(&self) -> &[SolutionComponentProjection] {
        &self.components
    }
    #[must_use]
    pub fn connections(&self) -> &[SolutionConnectionProjection] {
        &self.connections
    }
    #[must_use]
    pub const fn architecture_diagram(&self) -> &DiagramProjection {
        &self.architecture_diagram
    }
    #[must_use]
    pub const fn process_diagram(&self) -> &DiagramProjection {
        &self.process_diagram
    }
    #[must_use]
    pub fn risks(&self) -> &[String] {
        &self.risks
    }
    #[must_use]
    pub fn unresolved_items(&self) -> &[String] {
        &self.unresolved_items
    }
    #[must_use]
    pub fn task_proposals(&self) -> &[DeliveryTaskProposalProjection] {
        &self.task_proposals
    }
    #[must_use]
    pub const fn review_status(&self) -> SolutionReviewStatusProjection {
        self.review_status
    }
    #[must_use]
    pub const fn decision(&self) -> Option<SolutionReviewDecisionProjection> {
        self.decision
    }
    #[must_use]
    pub fn comments(&self) -> Option<&str> {
        self.comments.as_deref()
    }
    #[must_use]
    pub fn requested_changes(&self) -> Option<&[String]> {
        self.requested_changes.as_deref()
    }
    #[must_use]
    pub fn reviewer_id(&self) -> Option<&str> {
        self.reviewer_id.as_deref()
    }
    #[must_use]
    pub const fn reviewed_at(&self) -> Option<u64> {
        self.reviewed_at
    }
}

pub(super) fn project_current_solution_review(
    review: &ValidatedSolutionReviewSet,
) -> SolutionReviewProjection {
    let view = review.projection_view();
    SolutionReviewProjection {
        delivery_id: view.delivery_id.clone(),
        delivery_spec_id: view.delivery_spec_id.clone(),
        delivery_spec_revision: view.delivery_spec_revision,
        planning_stage_run_id: view.planning_stage_run_id.clone(),
        planning_session_binding_id: view.planning_session_binding_id.clone(),
        review_stage_run_id: view.review_stage_run_id.clone(),
        attention_item_id: view.attention_item_id.clone(),
        review_set_sha256: view.review_set_sha256.to_owned(),
        solution_id: view.solution_id.to_owned(),
        summary: view.summary.to_owned(),
        approach: view.approach.to_vec(),
        components: view.components.iter().map(project_component).collect(),
        connections: view.connections.iter().map(project_connection).collect(),
        architecture_diagram: project_diagram(view.architecture_diagram),
        process_diagram: project_diagram(view.process_diagram),
        risks: view.risks.to_vec(),
        unresolved_items: view.unresolved_items.to_vec(),
        task_proposals: view
            .task_proposals
            .iter()
            .map(project_task_proposal)
            .collect(),
        review_status: match view.review_status {
            ValidatedReviewStatus::Pending => SolutionReviewStatusProjection::Pending,
            ValidatedReviewStatus::Approved => SolutionReviewStatusProjection::Approved,
            ValidatedReviewStatus::ChangesRequested => {
                SolutionReviewStatusProjection::ChangesRequested
            }
            ValidatedReviewStatus::Rejected => SolutionReviewStatusProjection::Rejected,
        },
        decision: view.decision.map(|decision| match decision {
            ValidatedReviewDecision::Approve => SolutionReviewDecisionProjection::Approve,
            ValidatedReviewDecision::RequestChanges => {
                SolutionReviewDecisionProjection::RequestChanges
            }
            ValidatedReviewDecision::Reject => SolutionReviewDecisionProjection::Reject,
        }),
        comments: view.comments.map(str::to_owned),
        requested_changes: view.requested_changes.map(<[String]>::to_vec),
        reviewer_id: view.reviewer_id.map(str::to_owned),
        reviewed_at: view.reviewed_at,
    }
}

fn project_component(component: &ValidatedSolutionComponent) -> SolutionComponentProjection {
    SolutionComponentProjection {
        id: component.id.clone(),
        label: component.label.clone(),
        responsibility: component.responsibility.clone(),
        kind: match component.kind {
            ValidatedSolutionComponentKind::Component => SolutionComponentKind::Component,
            ValidatedSolutionComponentKind::External => SolutionComponentKind::External,
            ValidatedSolutionComponentKind::DataStore => SolutionComponentKind::DataStore,
        },
        trust_boundary: component.trust_boundary.clone(),
        unresolved: component.unresolved,
        repository_path_prefixes: component.repository_path_prefixes.clone(),
    }
}

fn project_connection(connection: &ValidatedSolutionConnection) -> SolutionConnectionProjection {
    SolutionConnectionProjection {
        id: connection.id.clone(),
        from: connection.from.clone(),
        to: connection.to.clone(),
        label: connection.label.clone(),
    }
}

fn project_diagram(diagram: &ValidatedDiagram) -> DiagramProjection {
    DiagramProjection {
        id: diagram.id.clone(),
        kind: match diagram.kind {
            ValidatedDiagramKind::SystemArchitecture => DiagramKind::SystemArchitecture,
            ValidatedDiagramKind::ProcessFlow => DiagramKind::ProcessFlow,
        },
        title: diagram.title.clone(),
        nodes: diagram.nodes.iter().map(project_diagram_node).collect(),
        edges: diagram.edges.iter().map(project_diagram_edge).collect(),
    }
}

fn project_diagram_node(node: &ValidatedDiagramNode) -> DiagramNodeProjection {
    DiagramNodeProjection {
        id: node.id.clone(),
        label: node.label.clone(),
        description: node.description.clone(),
        kind: match node.kind {
            ValidatedDiagramNodeKind::Interaction => DiagramNodeKind::Interaction,
            ValidatedDiagramNodeKind::DeliveryControl => DiagramNodeKind::DeliveryControl,
            ValidatedDiagramNodeKind::Execution => DiagramNodeKind::Execution,
            ValidatedDiagramNodeKind::Repository => DiagramNodeKind::Repository,
            ValidatedDiagramNodeKind::Component => DiagramNodeKind::Component,
            ValidatedDiagramNodeKind::External => DiagramNodeKind::External,
            ValidatedDiagramNodeKind::DataStore => DiagramNodeKind::DataStore,
            ValidatedDiagramNodeKind::Stage => DiagramNodeKind::Stage,
            ValidatedDiagramNodeKind::Decision => DiagramNodeKind::Decision,
        },
        trust_boundary: node.trust_boundary.clone(),
        unresolved: node.unresolved,
    }
}

fn project_diagram_edge(edge: &ValidatedDiagramEdge) -> DiagramEdgeProjection {
    DiagramEdgeProjection {
        id: edge.id.clone(),
        from: edge.from.clone(),
        to: edge.to.clone(),
        label: edge.label.clone(),
    }
}

fn project_task_proposal(proposal: &DeliveryTaskProposal) -> DeliveryTaskProposalProjection {
    DeliveryTaskProposalProjection {
        id: proposal.id().clone(),
        title: proposal.title().to_owned(),
        goal: proposal.goal().to_owned(),
        acceptance_criterion_ids: proposal.acceptance_criterion_ids().to_vec(),
        blocked_by_task_ids: proposal.blocked_by_task_ids().to_vec(),
    }
}
