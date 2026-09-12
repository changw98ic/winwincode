// SPDX-License-Identifier: Apache-2.0

//! Projection of current canonical Delivery-owned facts.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use winwincode_domain::{AttentionItemId, EvidenceId, ProductSessionId, WorkRunId};

use crate::domain::{
    AcceptanceCriterionId, AttentionItemStatus, AttentionItemType, CriterionResultId,
    CriterionVerdict, Delivery, DeliveryPublicationTarget, DeliverySourceRef, DeliverySpecId,
    DeliveryVerdictId, DeliveryVerdictStatus, EvidenceRefType, FrozenDeliveryCandidate,
    RepositoryRef, SessionBindingId, assert_frozen_candidate_current,
};

use super::{ProjectionError, ProjectionErrorCode};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptanceCriterionProjection {
    id: AcceptanceCriterionId,
    description: String,
    verification_method: Option<String>,
    required: bool,
}

impl AcceptanceCriterionProjection {
    #[must_use]
    pub fn id(&self) -> &AcceptanceCriterionId {
        &self.id
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn verification_method(&self) -> Option<&str> {
        self.verification_method.as_deref()
    }

    #[must_use]
    pub const fn required(&self) -> bool {
        self.required
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecProjection {
    id: DeliverySpecId,
    revision: u64,
    title: String,
    goal: String,
    scope: Vec<String>,
    out_of_scope: Vec<String>,
    constraints: Vec<String>,
    acceptance_criteria: Vec<AcceptanceCriterionProjection>,
    source_product_session_id: Option<ProductSessionId>,
    source_ref: Option<DeliverySourceRef>,
    publication_target: Option<DeliveryPublicationTarget>,
    repository: RepositoryRef,
    base_revision: String,
    max_rework_attempts: u64,
}

impl SpecProjection {
    #[must_use]
    pub fn id(&self) -> &DeliverySpecId {
        &self.id
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
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
    pub fn scope(&self) -> &[String] {
        &self.scope
    }

    #[must_use]
    pub fn out_of_scope(&self) -> &[String] {
        &self.out_of_scope
    }

    #[must_use]
    pub fn constraints(&self) -> &[String] {
        &self.constraints
    }

    #[must_use]
    pub fn acceptance_criteria(&self) -> &[AcceptanceCriterionProjection] {
        &self.acceptance_criteria
    }

    #[must_use]
    pub const fn source_product_session_id(&self) -> Option<&ProductSessionId> {
        self.source_product_session_id.as_ref()
    }

    #[must_use]
    pub const fn source_ref(&self) -> Option<&DeliverySourceRef> {
        self.source_ref.as_ref()
    }

    #[must_use]
    pub const fn publication_target(&self) -> Option<&DeliveryPublicationTarget> {
        self.publication_target.as_ref()
    }

    #[must_use]
    pub const fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    #[must_use]
    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    #[must_use]
    pub const fn max_rework_attempts(&self) -> u64 {
        self.max_rework_attempts
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequirementsProjection {
    spec: SpecProjection,
}

impl RequirementsProjection {
    #[must_use]
    pub const fn spec(&self) -> &SpecProjection {
        &self.spec
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionOptionProjection {
    id: String,
    label: String,
    description: String,
}

impl AttentionOptionProjection {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionItemProjection {
    id: AttentionItemId,
    delivery_spec_id: DeliverySpecId,
    work_run_id: Option<WorkRunId>,
    #[serde(rename = "type")]
    item_type: AttentionItemType,
    title: String,
    options: Vec<AttentionOptionProjection>,
    assigned_to: Option<String>,
    blocking: bool,
    status: AttentionItemStatus,
    resolution_summary: Option<String>,
    resolved_by: Option<String>,
    created_at: u64,
    resolved_at: Option<u64>,
}

impl AttentionItemProjection {
    #[must_use]
    pub fn id(&self) -> &AttentionItemId {
        &self.id
    }

    #[must_use]
    pub fn delivery_spec_id(&self) -> &DeliverySpecId {
        &self.delivery_spec_id
    }

    #[must_use]
    pub fn work_run_id(&self) -> Option<&WorkRunId> {
        self.work_run_id.as_ref()
    }

    #[must_use]
    pub const fn item_type(&self) -> AttentionItemType {
        self.item_type
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn options(&self) -> &[AttentionOptionProjection] {
        &self.options
    }

    #[must_use]
    pub fn assigned_to(&self) -> Option<&str> {
        self.assigned_to.as_deref()
    }

    #[must_use]
    pub const fn blocking(&self) -> bool {
        self.blocking
    }

    #[must_use]
    pub const fn status(&self) -> AttentionItemStatus {
        self.status
    }

    #[must_use]
    pub fn resolution_summary(&self) -> Option<&str> {
        self.resolution_summary.as_deref()
    }

    #[must_use]
    pub fn resolved_by(&self) -> Option<&str> {
        self.resolved_by.as_deref()
    }

    #[must_use]
    pub const fn created_at(&self) -> u64 {
        self.created_at
    }

    #[must_use]
    pub const fn resolved_at(&self) -> Option<u64> {
        self.resolved_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceProjection {
    id: EvidenceId,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    work_run_id: WorkRunId,
    session_binding_id: SessionBindingId,
    candidate_ref: String,
    #[serde(rename = "type")]
    evidence_type: EvidenceRefType,
    source_ref: String,
    created_at: u64,
}

impl EvidenceProjection {
    #[must_use]
    pub fn id(&self) -> &EvidenceId {
        &self.id
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
    pub fn work_run_id(&self) -> &WorkRunId {
        &self.work_run_id
    }

    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub fn session_binding_id(&self) -> &SessionBindingId {
        &self.session_binding_id
    }

    #[must_use]
    pub const fn evidence_type(&self) -> EvidenceRefType {
        self.evidence_type
    }

    #[must_use]
    pub fn source_ref(&self) -> &str {
        &self.source_ref
    }

    #[must_use]
    pub const fn created_at(&self) -> u64 {
        self.created_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentCandidateProjection {
    candidate_ref: String,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    producer_work_run_id: WorkRunId,
    producer_session_binding_id: SessionBindingId,
    candidate_commit_id: String,
    candidate_tree_id: String,
    diff_sha256: String,
    frozen_at: u64,
}

impl CurrentCandidateProjection {
    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
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
    pub fn producer_work_run_id(&self) -> &WorkRunId {
        &self.producer_work_run_id
    }

    #[must_use]
    pub fn producer_session_binding_id(&self) -> &SessionBindingId {
        &self.producer_session_binding_id
    }

    #[must_use]
    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    #[must_use]
    pub fn candidate_tree_id(&self) -> &str {
        &self.candidate_tree_id
    }

    #[must_use]
    pub fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

    #[must_use]
    pub const fn frozen_at(&self) -> u64 {
        self.frozen_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerdictCriterionProjection {
    result_id: CriterionResultId,
    criterion_id: AcceptanceCriterionId,
    verdict: CriterionVerdict,
    evidence_refs: Vec<EvidenceId>,
    explanation: String,
    evaluated_at: u64,
}

impl VerdictCriterionProjection {
    #[must_use]
    pub fn result_id(&self) -> &CriterionResultId {
        &self.result_id
    }

    #[must_use]
    pub fn criterion_id(&self) -> &AcceptanceCriterionId {
        &self.criterion_id
    }

    #[must_use]
    pub const fn verdict(&self) -> CriterionVerdict {
        self.verdict
    }

    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceId] {
        &self.evidence_refs
    }

    #[must_use]
    pub fn explanation(&self) -> &str {
        &self.explanation
    }

    #[must_use]
    pub const fn evaluated_at(&self) -> u64 {
        self.evaluated_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerdictProjection {
    id: DeliveryVerdictId,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    candidate_ref: String,
    status: DeliveryVerdictStatus,
    criteria: Vec<VerdictCriterionProjection>,
    unresolved_findings: Vec<String>,
    produced_at: u64,
}

impl VerdictProjection {
    #[must_use]
    pub fn id(&self) -> &DeliveryVerdictId {
        &self.id
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
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub const fn status(&self) -> DeliveryVerdictStatus {
        self.status
    }

    #[must_use]
    pub fn criteria(&self) -> &[VerdictCriterionProjection] {
        &self.criteria
    }

    #[must_use]
    pub fn unresolved_findings(&self) -> &[String] {
        &self.unresolved_findings
    }

    #[must_use]
    pub const fn produced_at(&self) -> u64 {
        self.produced_at
    }
}

pub(super) struct DeliverySections {
    pub(super) requirements: RequirementsProjection,
    pub(super) current_candidate: Option<CurrentCandidateProjection>,
    pub(super) attention: Vec<AttentionItemProjection>,
    pub(super) evidence: Vec<EvidenceProjection>,
    pub(super) verdict: Option<VerdictProjection>,
}

pub(super) fn project_delivery_sections(
    delivery: &Delivery,
    candidate: Option<&FrozenDeliveryCandidate>,
) -> Result<DeliverySections, ProjectionError> {
    let snapshot = delivery.snapshot();
    let current_candidate = validate_current_candidate(delivery, candidate)?;
    let current_candidate_ref = current_candidate
        .as_ref()
        .map(CurrentCandidateProjection::candidate_ref);

    let requirements = RequirementsProjection {
        spec: SpecProjection {
            id: snapshot.spec.id.clone(),
            revision: snapshot.spec.revision,
            title: snapshot.spec.title.clone(),
            goal: snapshot.spec.goal.clone(),
            scope: snapshot.spec.scope.clone(),
            out_of_scope: snapshot.spec.out_of_scope.clone(),
            constraints: snapshot.spec.constraints.clone(),
            acceptance_criteria: snapshot
                .spec
                .acceptance_criteria
                .iter()
                .map(|criterion| AcceptanceCriterionProjection {
                    id: criterion.id.clone(),
                    description: criterion.description.clone(),
                    verification_method: criterion.verification_method.clone(),
                    required: criterion.required,
                })
                .collect(),
            source_product_session_id: snapshot.spec.source_product_session_id.clone(),
            source_ref: snapshot.spec.source_ref.clone(),
            publication_target: snapshot.spec.publication_target.clone(),
            repository: snapshot.spec.repository.clone(),
            base_revision: snapshot.spec.base_revision.clone(),
            max_rework_attempts: snapshot.spec.max_rework_attempts,
        },
    };

    let evidence = project_current_evidence(delivery, current_candidate_ref);
    let attention = project_attention(delivery);
    let verdict = project_current_verdict(delivery, current_candidate_ref, &evidence)?;

    Ok(DeliverySections {
        requirements,
        current_candidate,
        attention,
        evidence,
        verdict,
    })
}

fn validate_current_candidate(
    delivery: &Delivery,
    candidate: Option<&FrozenDeliveryCandidate>,
) -> Result<Option<CurrentCandidateProjection>, ProjectionError> {
    let snapshot = delivery.snapshot();
    let Some(candidate) = candidate else {
        if snapshot.evidence.is_empty() && snapshot.verdict.is_none() {
            return Ok(None);
        }
        return Err(ProjectionError::new(
            ProjectionErrorCode::MissingCurrentCandidate,
            "candidate-bound Delivery facts require the sealed current candidate",
        ));
    };

    assert_frozen_candidate_current(delivery, candidate).map_err(|_| {
        ProjectionError::new(
            ProjectionErrorCode::StaleCandidate,
            "the supplied frozen candidate is not current for this Delivery",
        )
    })?;

    Ok(Some(CurrentCandidateProjection {
        candidate_ref: candidate.candidate_ref().into(),
        delivery_spec_id: candidate.delivery_spec_id().clone(),
        delivery_spec_revision: candidate.delivery_spec_revision(),
        producer_work_run_id: candidate.producer_work_run_id().clone(),
        producer_session_binding_id: candidate.producer_session_binding_id().clone(),
        candidate_commit_id: candidate.candidate_commit_id().into(),
        candidate_tree_id: candidate.candidate_tree_id().into(),
        diff_sha256: candidate.diff_sha256().into(),
        frozen_at: candidate.producer_finished_at_millis(),
    }))
}

fn project_current_evidence(
    delivery: &Delivery,
    current_candidate_ref: Option<&str>,
) -> Vec<EvidenceProjection> {
    let snapshot = delivery.snapshot();
    let mut evidence: Vec<_> = snapshot
        .evidence
        .iter()
        .filter(|reference| {
            current_candidate_ref == Some(reference.candidate_ref.as_str())
                && reference.delivery_spec_id == snapshot.spec.id
                && reference.delivery_spec_revision == snapshot.spec.revision
        })
        .map(|reference| EvidenceProjection {
            id: reference.id.clone(),
            delivery_spec_id: reference.delivery_spec_id.clone(),
            delivery_spec_revision: reference.delivery_spec_revision,
            work_run_id: reference.work_run_id.clone(),
            session_binding_id: reference.session_binding_id.clone(),
            candidate_ref: reference.candidate_ref.clone(),
            evidence_type: reference.evidence_type,
            source_ref: reference.source_ref.clone(),
            created_at: reference.created_at_millis,
        })
        .collect();
    evidence.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
    evidence
}

fn project_attention(delivery: &Delivery) -> Vec<AttentionItemProjection> {
    let snapshot = delivery.snapshot();
    let mut attention: Vec<_> = snapshot
        .attention_items
        .iter()
        .filter(|item| item.delivery_id == snapshot.id && item.delivery_spec_id == snapshot.spec.id)
        .map(|item| {
            let mut options: Vec<_> = item
                .options
                .iter()
                .map(|option| AttentionOptionProjection {
                    id: option.id.clone(),
                    label: option.label.clone(),
                    description: option.description.clone(),
                })
                .collect();
            options.sort_by(|left, right| left.id.cmp(&right.id));
            let resolution_summary = match item.status {
                AttentionItemStatus::Open => None,
                AttentionItemStatus::Resolved => Some("resolved".into()),
                AttentionItemStatus::Dismissed => Some("dismissed".into()),
            };
            AttentionItemProjection {
                id: item.id.clone(),
                delivery_spec_id: item.delivery_spec_id.clone(),
                work_run_id: item.work_run_id.clone(),
                item_type: item.item_type,
                title: item.title.clone(),
                options,
                assigned_to: item.assigned_to.clone(),
                blocking: item.blocking,
                status: item.status,
                resolution_summary,
                resolved_by: item.resolved_by.clone(),
                created_at: item.created_at_millis,
                resolved_at: item.resolved_at_millis,
            }
        })
        .collect();
    attention.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
    attention
}

fn project_current_verdict(
    delivery: &Delivery,
    current_candidate_ref: Option<&str>,
    evidence: &[EvidenceProjection],
) -> Result<Option<VerdictProjection>, ProjectionError> {
    let snapshot = delivery.snapshot();
    let Some(verdict) = &snapshot.verdict else {
        return Ok(None);
    };
    if current_candidate_ref != Some(verdict.candidate_ref.as_str()) {
        return Err(ProjectionError::new(
            ProjectionErrorCode::InconsistentCurrentVerdict,
            "the canonical DeliveryVerdict does not identify the sealed current candidate",
        ));
    }

    let available_evidence: HashSet<_> = evidence.iter().map(|entry| entry.id.0.as_str()).collect();
    let mut results_by_criterion: HashMap<&str, Vec<_>> = HashMap::new();
    for result in &verdict.criteria {
        results_by_criterion
            .entry(result.criterion_id.0.as_str())
            .or_default()
            .push(result);
    }
    let mut criteria = Vec::with_capacity(snapshot.spec.acceptance_criteria.len());
    for criterion in &snapshot.spec.acceptance_criteria {
        let Some(results) = results_by_criterion.get(criterion.id.0.as_str()) else {
            return Err(inconsistent_verdict(
                "a current acceptance criterion is missing",
            ));
        };
        let [result] = results.as_slice() else {
            return Err(inconsistent_verdict(
                "a current acceptance criterion is evaluated more than once",
            ));
        };
        if result
            .evidence_refs
            .iter()
            .any(|id| !available_evidence.contains(id.0.as_str()))
        {
            return Err(inconsistent_verdict(
                "a criterion cites Evidence outside the current candidate projection",
            ));
        }
        let mut evidence_refs = result.evidence_refs.clone();
        evidence_refs.sort_by(|left, right| left.0.cmp(&right.0));
        criteria.push(VerdictCriterionProjection {
            result_id: result.id.clone(),
            criterion_id: result.criterion_id.clone(),
            verdict: result.verdict,
            evidence_refs,
            explanation: result.explanation.clone(),
            evaluated_at: result.evaluated_at_millis,
        });
    }
    if results_by_criterion.len() != criteria.len() {
        return Err(inconsistent_verdict(
            "the verdict contains a foreign acceptance criterion",
        ));
    }
    let mut unresolved_findings = verdict.unresolved_findings.clone();
    unresolved_findings.sort();
    unresolved_findings.dedup();

    Ok(Some(VerdictProjection {
        id: verdict.id.clone(),
        delivery_spec_id: verdict.delivery_spec_id.clone(),
        delivery_spec_revision: snapshot.spec.revision,
        candidate_ref: verdict.candidate_ref.clone(),
        status: verdict.status,
        criteria,
        unresolved_findings,
        produced_at: verdict.produced_at_millis,
    }))
}

fn inconsistent_verdict(message: &str) -> ProjectionError {
    ProjectionError::new(ProjectionErrorCode::InconsistentCurrentVerdict, message)
}
