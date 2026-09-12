// SPDX-License-Identifier: Apache-2.0

//! Safe, deterministic `StrongFlow` read models.
//!
//! This module is deliberately one-way: it accepts already-authoritative
//! Delivery-domain facts and returns serializable values. It owns no command,
//! scheduler, persistence, runtime-log, or credential port.

pub mod delivery;
pub mod redaction;
pub mod runtime;
pub mod solution;

use std::{error::Error, fmt};

use serde::Serialize;

use crate::{
    application::solution_review::{SolutionReviewErrorCode, resolve_current_solution_review},
    domain::{AttentionItemStatus, Delivery, FrozenDeliveryCandidate},
};
use winwincode_domain::{WorkItem, WorkItemState};

pub use delivery::{
    AcceptanceCriterionProjection, AttentionItemProjection, AttentionOptionProjection,
    CurrentCandidateProjection, EvidenceProjection, RequirementsProjection, SpecProjection,
    VerdictCriterionProjection, VerdictProjection,
};
pub use solution::{
    DiagramEdgeProjection, DiagramKind, DiagramNodeKind, DiagramNodeProjection, DiagramProjection,
    SolutionComponentKind, SolutionComponentProjection, SolutionConnectionProjection,
    SolutionReviewDecisionProjection, SolutionReviewProjection, SolutionReviewStatusProjection,
    WorkItemProposalProjection,
};

/// The only caller-selected inputs to the Delivery detail read model.
///
/// Candidate values are sealed domain facts. The current solution review is
/// rebuilt internally from canonical Delivery Attention facts rather than
/// accepted from a caller, DTO, or Worker message.
#[derive(Clone, Copy)]
pub struct ProjectionInput<'facts> {
    delivery: &'facts Delivery,
    candidate: Option<&'facts FrozenDeliveryCandidate>,
}

impl<'facts> ProjectionInput<'facts> {
    #[must_use]
    pub const fn new(delivery: &'facts Delivery) -> Self {
        Self {
            delivery,
            candidate: None,
        }
    }

    #[must_use]
    pub const fn with_candidate(mut self, candidate: &'facts FrozenDeliveryCandidate) -> Self {
        self.candidate = Some(candidate);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionErrorCode {
    MissingCurrentCandidate,
    StaleCandidate,
    InvalidSessionBinding,
    StaleSolutionReview,
    InconsistentCurrentVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionError {
    code: ProjectionErrorCode,
    message: String,
}

impl ProjectionError {
    #[must_use]
    pub const fn code(&self) -> ProjectionErrorCode {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    pub(super) fn new(code: ProjectionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProjectionError {}

/// Complete Delivery-owned `StrongFlow` detail without the mutable Delivery.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryProjection {
    delivery_id: winwincode_domain::DeliveryId,
    delivery_revision: u64,
    status: WorkItemState,
    requirements: RequirementsProjection,
    solution_review: Option<SolutionReviewProjection>,
    work_items: Vec<WorkItem>,
    attention: Vec<AttentionItemProjection>,
    evidence: Vec<EvidenceProjection>,
    current_candidate: Option<CurrentCandidateProjection>,
    verdict: Option<VerdictProjection>,
}

impl DeliveryProjection {
    #[must_use]
    pub fn delivery_id(&self) -> &winwincode_domain::DeliveryId {
        &self.delivery_id
    }

    #[must_use]
    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    #[must_use]
    pub const fn status(&self) -> &WorkItemState {
        &self.status
    }

    #[must_use]
    pub const fn requirements(&self) -> &RequirementsProjection {
        &self.requirements
    }

    #[must_use]
    pub const fn solution_review(&self) -> Option<&SolutionReviewProjection> {
        self.solution_review.as_ref()
    }

    #[must_use]
    pub fn work_items(&self) -> &[WorkItem] {
        &self.work_items
    }

    #[must_use]
    pub fn attention(&self) -> &[AttentionItemProjection] {
        &self.attention
    }

    #[must_use]
    pub fn evidence(&self) -> &[EvidenceProjection] {
        &self.evidence
    }

    #[must_use]
    pub const fn current_candidate(&self) -> Option<&CurrentCandidateProjection> {
        self.current_candidate.as_ref()
    }

    #[must_use]
    pub const fn verdict(&self) -> Option<&VerdictProjection> {
        self.verdict.as_ref()
    }

    /// Deterministic JSON bytes for replay and reload equality checks.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if JSON encoding fails.
    pub fn encode_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

/// Builds the Delivery-owned `StrongFlow` detail projection.
///
/// # Errors
///
/// Rejects a stale candidate, a stale solution review, or a canonical verdict that does not identify the supplied
/// current candidate.
pub fn project_delivery_detail(
    input: ProjectionInput<'_>,
) -> Result<DeliveryProjection, ProjectionError> {
    let sections = delivery::project_delivery_sections(input.delivery, input.candidate)?;
    let solution_review = resolve_current_solution_review(input.delivery)
        .map_err(|error| {
            let code = match error.code() {
                SolutionReviewErrorCode::InvalidEncoding
                | SolutionReviewErrorCode::InvalidContent
                | SolutionReviewErrorCode::StaleAuthority
                | SolutionReviewErrorCode::AmbiguousCurrentReview => {
                    ProjectionErrorCode::StaleSolutionReview
                }
            };
            ProjectionError::new(code, error.message())
        })?
        .as_ref()
        .map(solution::project_current_solution_review);

    Ok(DeliveryProjection {
        delivery_id: input.delivery.id().clone(),
        delivery_revision: input.delivery.revision(),
        status: input.delivery.snapshot().work_run_aggregate.summary_state(
            input
                .delivery
                .snapshot()
                .attention_items
                .iter()
                .any(|item| item.status == AttentionItemStatus::Open),
        ),
        requirements: sections.requirements,
        solution_review,
        work_items: input.delivery.snapshot().work_run_aggregate.items.clone(),
        attention: sections.attention,
        evidence: sections.evidence,
        current_candidate: sections.current_candidate,
        verdict: sections.verdict,
    })
}
