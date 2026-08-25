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
    domain::{Delivery, DeliveryStatus, FrozenDeliveryCandidate},
};

pub use delivery::{
    AcceptanceCriterionProjection, AttentionItemProjection, AttentionOptionProjection,
    CurrentCandidateProjection, DeliveryTaskProjection, EvidenceProjection, RequirementsProjection,
    SessionBindingProjection, SpecProjection, StageProjection, VerdictCriterionProjection,
    VerdictProjection,
};
pub use solution::{
    DeliveryTaskProposalProjection, DiagramEdgeProjection, DiagramKind, DiagramNodeKind,
    DiagramNodeProjection, DiagramProjection, SolutionComponentKind, SolutionComponentProjection,
    SolutionConnectionProjection, SolutionReviewDecisionProjection, SolutionReviewProjection,
    SolutionReviewStatusProjection,
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
    pub const fn code(&self) -> ProjectionErrorCode {
        self.code
    }

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryProjection {
    delivery_id: winwincode_domain::DeliveryId,
    delivery_revision: u64,
    status: DeliveryStatus,
    requirements: RequirementsProjection,
    solution_review: Option<SolutionReviewProjection>,
    stages: Vec<StageProjection>,
    tasks: Vec<DeliveryTaskProjection>,
    attention: Vec<AttentionItemProjection>,
    evidence: Vec<EvidenceProjection>,
    current_candidate: Option<CurrentCandidateProjection>,
    verdict: Option<VerdictProjection>,
}

impl DeliveryProjection {
    pub fn delivery_id(&self) -> &winwincode_domain::DeliveryId {
        &self.delivery_id
    }

    pub const fn delivery_revision(&self) -> u64 {
        self.delivery_revision
    }

    pub const fn status(&self) -> DeliveryStatus {
        self.status
    }

    pub const fn requirements(&self) -> &RequirementsProjection {
        &self.requirements
    }

    pub const fn solution_review(&self) -> Option<&SolutionReviewProjection> {
        self.solution_review.as_ref()
    }

    pub fn stages(&self) -> &[StageProjection] {
        &self.stages
    }

    pub fn tasks(&self) -> &[DeliveryTaskProjection] {
        &self.tasks
    }

    pub fn attention(&self) -> &[AttentionItemProjection] {
        &self.attention
    }

    pub fn evidence(&self) -> &[EvidenceProjection] {
        &self.evidence
    }

    pub const fn current_candidate(&self) -> Option<&CurrentCandidateProjection> {
        self.current_candidate.as_ref()
    }

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
/// Rejects a stale candidate, a missing or conflicting `StageRun` binding, a stale solution
/// review, or a canonical verdict that does not identify the supplied
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
        status: input.delivery.snapshot().status,
        requirements: sections.requirements,
        solution_review,
        stages: sections.stages,
        tasks: sections.tasks,
        attention: sections.attention,
        evidence: sections.evidence,
        current_candidate: sections.current_candidate,
        verdict: sections.verdict,
    })
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        CodexThreadId, DeliveryTaskId, ExecutionJobId, ProductSessionId, StageRunId,
        WorkerSessionId,
    };

    use super::*;
    use crate::domain::{SessionBindingId, test_fixture};

    fn unordered_deliveries() -> (Delivery, Delivery) {
        let mut first = test_fixture();
        first.status = DeliveryStatus::Verifying;
        first.evidence.clear();
        first.verdict = None;

        let mut second_task = first.tasks[0].clone();
        second_task.id = DeliveryTaskId("delivery-task-ui".into());
        second_task.title = "Invitation UI".into();
        second_task.owner = Some("frontend-owner".into());
        first.tasks.push(second_task);

        let mut second_run = first.stage_runs[0].clone();
        second_run.id = StageRunId("stage-verification-2".into());
        second_run.delivery_task_id = Some(DeliveryTaskId("delivery-task-ui".into()));
        second_run.started_at_millis += 100;
        second_run.finished_at_millis = second_run.finished_at_millis.map(|value| value + 100);
        first.stage_runs.push(second_run);

        let mut second_binding = first.session_bindings[0].clone();
        second_binding.id = SessionBindingId("binding-verifier-2".into());
        second_binding.delivery_task_id = Some(DeliveryTaskId("delivery-task-ui".into()));
        second_binding.stage_run_id = StageRunId("stage-verification-2".into());
        second_binding.product_session_id = ProductSessionId("product-session-verifier-2".into());
        second_binding.execution_job_id = ExecutionJobId("execution-job-verifier-2".into());
        second_binding.worker_session_id =
            Some(WorkerSessionId("worker-session-verifier-2".into()));
        second_binding.codex_thread_id = Some(CodexThreadId("codex-thread-verifier-2".into()));
        second_binding.bound_at_millis += 100;
        first.session_bindings.push(second_binding);

        let first = Delivery::try_from_snapshot(first).expect("first Delivery");
        let mut second = first.clone().into_snapshot();
        second.tasks.reverse();
        second.stage_runs.reverse();
        second.session_bindings.reverse();
        let second = Delivery::try_from_snapshot(second).expect("reordered Delivery");
        (first, second)
    }

    #[test]
    fn projection_is_read_only_over_authoritative_sources() {
        let (delivery, _) = unordered_deliveries();
        let before = delivery.encode_json().expect("before");

        let _ = project_delivery_detail(ProjectionInput::new(&delivery)).expect("projection");

        assert_eq!(delivery.encode_json().expect("after"), before);
    }

    #[test]
    fn projection_order_is_deterministic() {
        let (first, second) = unordered_deliveries();

        let first = project_delivery_detail(ProjectionInput::new(&first))
            .expect("first projection")
            .encode_json()
            .expect("first JSON");
        let second = project_delivery_detail(ProjectionInput::new(&second))
            .expect("second projection")
            .encode_json()
            .expect("second JSON");

        assert_eq!(first, second);
    }
}
