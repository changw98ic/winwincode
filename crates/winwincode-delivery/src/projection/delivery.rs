// SPDX-License-Identifier: Apache-2.0

//! Projection of current canonical Delivery-owned facts.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use winwincode_domain::{
    AttentionItemId, CodexThreadId, DeliveryTaskId, EvidenceId, ExecutionJobId, FencingToken,
    LeaseId, ProductSessionId, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

use crate::domain::{
    AcceptanceCriterionId, AttentionItemStatus, AttentionItemType, CriterionResultId,
    CriterionVerdict, Delivery, DeliveryPublicationTarget, DeliverySourceRef, DeliverySpecId,
    DeliveryStage, DeliveryTaskStatus, DeliveryVerdictId, DeliveryVerdictStatus, EvidenceRefType,
    FrozenDeliveryCandidate, RepositoryRef, SessionBindingId, SessionBindingSourceProvenance,
    StageRunActorType, StageRunStatus, assert_frozen_candidate_current,
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
    pub fn id(&self) -> &AcceptanceCriterionId {
        &self.id
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn verification_method(&self) -> Option<&str> {
        self.verification_method.as_deref()
    }

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
    source_ref: Option<DeliverySourceRef>,
    publication_target: Option<DeliveryPublicationTarget>,
    repository: RepositoryRef,
    base_revision: String,
    max_rework_attempts: u64,
}

impl SpecProjection {
    pub fn id(&self) -> &DeliverySpecId {
        &self.id
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn goal(&self) -> &str {
        &self.goal
    }

    pub fn scope(&self) -> &[String] {
        &self.scope
    }

    pub fn out_of_scope(&self) -> &[String] {
        &self.out_of_scope
    }

    pub fn constraints(&self) -> &[String] {
        &self.constraints
    }

    pub fn acceptance_criteria(&self) -> &[AcceptanceCriterionProjection] {
        &self.acceptance_criteria
    }

    pub const fn source_ref(&self) -> Option<&DeliverySourceRef> {
        self.source_ref.as_ref()
    }

    pub const fn publication_target(&self) -> Option<&DeliveryPublicationTarget> {
        self.publication_target.as_ref()
    }

    pub const fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

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
    pub const fn spec(&self) -> &SpecProjection {
        &self.spec
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBindingProjection {
    binding_id: SessionBindingId,
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    worker_session_id: Option<WorkerSessionId>,
    codex_thread_id: Option<CodexThreadId>,
    worker_id: Option<WorkerId>,
    worker_instance_id: Option<WorkerInstanceId>,
    lease_id: Option<LeaseId>,
    attempt: u64,
    fencing_token: Option<FencingToken>,
    source_provenance: SessionBindingSourceProvenance,
    bound_at: u64,
}

impl SessionBindingProjection {
    pub fn binding_id(&self) -> &SessionBindingId {
        &self.binding_id
    }

    pub fn product_session_id(&self) -> &ProductSessionId {
        &self.product_session_id
    }

    pub fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    pub fn worker_session_id(&self) -> Option<&WorkerSessionId> {
        self.worker_session_id.as_ref()
    }

    pub fn codex_thread_id(&self) -> Option<&CodexThreadId> {
        self.codex_thread_id.as_ref()
    }

    pub fn worker_id(&self) -> Option<&WorkerId> {
        self.worker_id.as_ref()
    }

    pub fn worker_instance_id(&self) -> Option<&WorkerInstanceId> {
        self.worker_instance_id.as_ref()
    }

    pub fn lease_id(&self) -> Option<&LeaseId> {
        self.lease_id.as_ref()
    }

    pub const fn attempt(&self) -> u64 {
        self.attempt
    }

    pub fn fencing_token(&self) -> Option<&FencingToken> {
        self.fencing_token.as_ref()
    }

    pub const fn source_provenance(&self) -> &SessionBindingSourceProvenance {
        &self.source_provenance
    }

    pub const fn bound_at(&self) -> u64 {
        self.bound_at
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StageProjection {
    id: StageRunId,
    delivery_task_id: Option<DeliveryTaskId>,
    stage: DeliveryStage,
    actor_type: StageRunActorType,
    role: String,
    status: StageRunStatus,
    attempt: u64,
    started_at: u64,
    finished_at: Option<u64>,
    session_binding: Option<SessionBindingProjection>,
}

impl StageProjection {
    pub fn id(&self) -> &StageRunId {
        &self.id
    }

    pub fn delivery_task_id(&self) -> Option<&DeliveryTaskId> {
        self.delivery_task_id.as_ref()
    }

    pub const fn stage(&self) -> DeliveryStage {
        self.stage
    }

    pub const fn actor_type(&self) -> StageRunActorType {
        self.actor_type
    }

    pub fn role(&self) -> &str {
        &self.role
    }

    pub const fn status(&self) -> StageRunStatus {
        self.status
    }

    pub const fn attempt(&self) -> u64 {
        self.attempt
    }

    pub const fn started_at(&self) -> u64 {
        self.started_at
    }

    pub const fn finished_at(&self) -> Option<u64> {
        self.finished_at
    }

    pub const fn session_binding(&self) -> Option<&SessionBindingProjection> {
        self.session_binding.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryTaskProjection {
    id: DeliveryTaskId,
    title: String,
    goal: String,
    acceptance_criterion_ids: Vec<AcceptanceCriterionId>,
    blocked_by_task_ids: Vec<DeliveryTaskId>,
    owner: Option<String>,
    status: DeliveryTaskStatus,
    stage_run_ids: Vec<StageRunId>,
    evidence_refs: Vec<EvidenceId>,
}

impl DeliveryTaskProjection {
    pub fn id(&self) -> &DeliveryTaskId {
        &self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn goal(&self) -> &str {
        &self.goal
    }

    pub fn acceptance_criterion_ids(&self) -> &[AcceptanceCriterionId] {
        &self.acceptance_criterion_ids
    }

    pub fn blocked_by_task_ids(&self) -> &[DeliveryTaskId] {
        &self.blocked_by_task_ids
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    pub const fn status(&self) -> DeliveryTaskStatus {
        self.status
    }

    pub fn stage_run_ids(&self) -> &[StageRunId] {
        &self.stage_run_ids
    }

    pub fn evidence_refs(&self) -> &[EvidenceId] {
        &self.evidence_refs
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
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn description(&self) -> &str {
        &self.description
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionItemProjection {
    id: AttentionItemId,
    delivery_spec_id: DeliverySpecId,
    stage_run_id: Option<StageRunId>,
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
    pub fn id(&self) -> &AttentionItemId {
        &self.id
    }

    pub fn delivery_spec_id(&self) -> &DeliverySpecId {
        &self.delivery_spec_id
    }

    pub fn stage_run_id(&self) -> Option<&StageRunId> {
        self.stage_run_id.as_ref()
    }

    pub const fn item_type(&self) -> AttentionItemType {
        self.item_type
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn options(&self) -> &[AttentionOptionProjection] {
        &self.options
    }

    pub fn assigned_to(&self) -> Option<&str> {
        self.assigned_to.as_deref()
    }

    pub const fn blocking(&self) -> bool {
        self.blocking
    }

    pub const fn status(&self) -> AttentionItemStatus {
        self.status
    }

    pub fn resolution_summary(&self) -> Option<&str> {
        self.resolution_summary.as_deref()
    }

    pub fn resolved_by(&self) -> Option<&str> {
        self.resolved_by.as_deref()
    }

    pub const fn created_at(&self) -> u64 {
        self.created_at
    }

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
    stage_run_id: StageRunId,
    session_binding_id: SessionBindingId,
    candidate_ref: String,
    #[serde(rename = "type")]
    evidence_type: EvidenceRefType,
    source_ref: String,
    created_at: u64,
}

impl EvidenceProjection {
    pub fn id(&self) -> &EvidenceId {
        &self.id
    }

    pub fn delivery_spec_id(&self) -> &DeliverySpecId {
        &self.delivery_spec_id
    }

    pub const fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }

    pub fn stage_run_id(&self) -> &StageRunId {
        &self.stage_run_id
    }

    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    pub fn session_binding_id(&self) -> &SessionBindingId {
        &self.session_binding_id
    }

    pub const fn evidence_type(&self) -> EvidenceRefType {
        self.evidence_type
    }

    pub fn source_ref(&self) -> &str {
        &self.source_ref
    }

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
    producer_stage_run_id: StageRunId,
    producer_session_binding_id: SessionBindingId,
    candidate_commit_id: String,
    candidate_tree_id: String,
    diff_sha256: String,
    frozen_at: u64,
}

impl CurrentCandidateProjection {
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    pub fn delivery_spec_id(&self) -> &DeliverySpecId {
        &self.delivery_spec_id
    }

    pub const fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }

    pub fn producer_stage_run_id(&self) -> &StageRunId {
        &self.producer_stage_run_id
    }

    pub fn producer_session_binding_id(&self) -> &SessionBindingId {
        &self.producer_session_binding_id
    }

    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    pub fn candidate_tree_id(&self) -> &str {
        &self.candidate_tree_id
    }

    pub fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

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
    pub fn result_id(&self) -> &CriterionResultId {
        &self.result_id
    }

    pub fn criterion_id(&self) -> &AcceptanceCriterionId {
        &self.criterion_id
    }

    pub const fn verdict(&self) -> CriterionVerdict {
        self.verdict
    }

    pub fn evidence_refs(&self) -> &[EvidenceId] {
        &self.evidence_refs
    }

    pub fn explanation(&self) -> &str {
        &self.explanation
    }

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
    pub fn id(&self) -> &DeliveryVerdictId {
        &self.id
    }

    pub fn delivery_spec_id(&self) -> &DeliverySpecId {
        &self.delivery_spec_id
    }

    pub const fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }

    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    pub const fn status(&self) -> DeliveryVerdictStatus {
        self.status
    }

    pub fn criteria(&self) -> &[VerdictCriterionProjection] {
        &self.criteria
    }

    pub fn unresolved_findings(&self) -> &[String] {
        &self.unresolved_findings
    }

    pub const fn produced_at(&self) -> u64 {
        self.produced_at
    }
}

pub(super) struct DeliverySections {
    pub(super) requirements: RequirementsProjection,
    pub(super) current_candidate: Option<CurrentCandidateProjection>,
    pub(super) stages: Vec<StageProjection>,
    pub(super) tasks: Vec<DeliveryTaskProjection>,
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
            source_ref: snapshot.spec.source_ref.clone(),
            publication_target: snapshot.spec.publication_target.clone(),
            repository: snapshot.spec.repository.clone(),
            base_revision: snapshot.spec.base_revision.clone(),
            max_rework_attempts: snapshot.spec.max_rework_attempts,
        },
    };

    let stages = project_stages(delivery)?;
    let evidence = project_current_evidence(delivery, current_candidate_ref);
    let tasks = project_tasks(delivery, &stages, &evidence);
    let attention = project_attention(delivery);
    let verdict = project_current_verdict(delivery, current_candidate_ref, &evidence)?;

    Ok(DeliverySections {
        requirements,
        current_candidate,
        stages,
        tasks,
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
        producer_stage_run_id: candidate.producer_stage_run_id().clone(),
        producer_session_binding_id: candidate.producer_session_binding_id().clone(),
        candidate_commit_id: candidate.candidate_commit_id().into(),
        candidate_tree_id: candidate.candidate_tree_id().into(),
        diff_sha256: candidate.diff_sha256().into(),
        frozen_at: candidate.producer_finished_at_millis(),
    }))
}

fn project_stages(delivery: &Delivery) -> Result<Vec<StageProjection>, ProjectionError> {
    let snapshot = delivery.snapshot();
    let mut bindings_by_run: HashMap<&str, Vec<_>> = HashMap::new();
    for binding in &snapshot.session_bindings {
        bindings_by_run
            .entry(binding.stage_run_id.0.as_str())
            .or_default()
            .push(binding);
    }

    let mut stages = Vec::with_capacity(snapshot.stage_runs.len());
    for run in &snapshot.stage_runs {
        let bindings = bindings_by_run
            .get(run.id.0.as_str())
            .map_or(&[][..], Vec::as_slice);
        let binding_count_is_invalid = match run.actor_type {
            StageRunActorType::Codex => bindings.len() != 1,
            StageRunActorType::Human => !bindings.is_empty(),
        };
        if binding_count_is_invalid {
            return Err(ProjectionError::new(
                ProjectionErrorCode::InvalidSessionBinding,
                format!(
                    "StageRun {} does not have the exact SessionBinding count for its actor",
                    run.id.0
                ),
            ));
        }
        let session_binding = bindings.first().map(|binding| SessionBindingProjection {
            binding_id: binding.id.clone(),
            product_session_id: binding.product_session_id.clone(),
            execution_job_id: binding.execution_job_id.clone(),
            worker_session_id: binding.worker_session_id.clone(),
            codex_thread_id: binding.codex_thread_id.clone(),
            worker_id: binding.worker_id.clone(),
            worker_instance_id: binding.worker_instance_id.clone(),
            lease_id: binding.lease_id.clone(),
            attempt: binding.attempt,
            fencing_token: binding.fencing_token.clone(),
            source_provenance: binding.source_provenance.clone(),
            bound_at: binding.bound_at_millis,
        });
        stages.push(StageProjection {
            id: run.id.clone(),
            delivery_task_id: run.delivery_task_id.clone(),
            stage: run.stage,
            actor_type: run.actor_type,
            role: run.role.clone(),
            status: run.status,
            attempt: run.attempt,
            started_at: run.started_at_millis,
            finished_at: run.finished_at_millis,
            session_binding,
        });
    }
    stages.sort_by(|left, right| {
        left.started_at
            .cmp(&right.started_at)
            .then_with(|| left.attempt.cmp(&right.attempt))
            .then_with(|| left.id.0.cmp(&right.id.0))
    });
    Ok(stages)
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
            stage_run_id: reference.stage_run_id.clone(),
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

fn project_tasks(
    delivery: &Delivery,
    stages: &[StageProjection],
    evidence: &[EvidenceProjection],
) -> Vec<DeliveryTaskProjection> {
    let mut tasks: Vec<_> = delivery
        .snapshot()
        .tasks
        .iter()
        .map(|task| {
            let mut stage_run_ids: Vec<_> = stages
                .iter()
                .filter(|run| run.delivery_task_id.as_ref() == Some(&task.id))
                .map(|run| run.id.clone())
                .collect();
            stage_run_ids.sort_by(|left, right| left.0.cmp(&right.0));
            let owned_runs: HashSet<_> = stage_run_ids.iter().map(|id| id.0.as_str()).collect();
            let mut evidence_refs: Vec<_> = evidence
                .iter()
                .filter(|reference| owned_runs.contains(reference.stage_run_id.0.as_str()))
                .map(|reference| reference.id.clone())
                .collect();
            evidence_refs.sort_by(|left, right| left.0.cmp(&right.0));
            let mut acceptance_criterion_ids = task.acceptance_criterion_ids.clone();
            acceptance_criterion_ids.sort_by(|left, right| left.0.cmp(&right.0));
            let mut blocked_by_task_ids = task.blocked_by_task_ids.clone();
            blocked_by_task_ids.sort_by(|left, right| left.0.cmp(&right.0));
            DeliveryTaskProjection {
                id: task.id.clone(),
                title: task.title.clone(),
                goal: task.goal.clone(),
                acceptance_criterion_ids,
                blocked_by_task_ids,
                owner: task.owner.clone(),
                status: task.status,
                stage_run_ids,
                evidence_refs,
            }
        })
        .collect();
    tasks.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    tasks
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
                stage_run_id: item.stage_run_id.clone(),
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

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        AttentionItemId, CodexThreadId, DeliveryTaskId, EvidenceId, ExecutionJobId,
        ProductSessionId, StageRunId, WorkerSessionId,
    };

    use super::*;
    use crate::domain::{
        AttentionItem, AttentionItemStatus, AttentionItemType, DeliveryStatus, test_fixture,
    };
    use crate::projection::{ProjectionInput, project_delivery_detail};

    fn delivery_without_candidate_facts() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        Delivery::try_from_snapshot(snapshot).expect("Delivery without candidate facts")
    }

    #[test]
    fn requirements_projection_uses_current_delivery_spec() {
        let mut snapshot = delivery_without_candidate_facts().into_snapshot();
        snapshot.spec.revision = 2;
        snapshot.spec.title = "Current invitation requirements".into();
        snapshot.revision += 1;
        let delivery = Delivery::try_from_snapshot(snapshot).expect("revised Delivery");

        let projection = project_delivery_detail(ProjectionInput::new(&delivery))
            .expect("requirements projection");

        assert_eq!(projection.requirements().spec().revision(), 2);
        assert_eq!(
            projection.requirements().spec().title(),
            "Current invitation requirements"
        );
    }

    #[test]
    fn stage_projection_keeps_stage_and_session_identities_distinct() {
        let delivery = delivery_without_candidate_facts();
        let projection =
            project_delivery_detail(ProjectionInput::new(&delivery)).expect("stage projection");
        let binding = projection.stages()[0]
            .session_binding()
            .expect("exact binding");
        assert_ne!(binding.product_session_id().0, binding.execution_job_id().0);
        assert_ne!(
            binding.worker_session_id().expect("WorkerSession").0,
            binding.codex_thread_id().expect("CodexThread").0
        );

        let mut ambiguous = delivery.into_snapshot();
        let mut duplicate = ambiguous.session_bindings[0].clone();
        duplicate.id = SessionBindingId("binding-verifier-duplicate".into());
        duplicate.product_session_id = ProductSessionId("product-verifier-duplicate".into());
        duplicate.execution_job_id = ExecutionJobId("job-verifier-duplicate".into());
        duplicate.worker_session_id = Some(WorkerSessionId("worker-verifier-duplicate".into()));
        duplicate.codex_thread_id = Some(CodexThreadId("thread-verifier-duplicate".into()));
        duplicate = duplicate.with_test_authority("projection-verifier-duplicate", 1);
        ambiguous.session_bindings.push(duplicate);
        let ambiguous = Delivery::try_from_snapshot(ambiguous).expect("canonical ambiguity");
        assert_eq!(
            project_delivery_detail(ProjectionInput::new(&ambiguous))
                .expect_err("ambiguous StageRun binding")
                .code(),
            ProjectionErrorCode::InvalidSessionBinding
        );
    }

    #[test]
    fn task_projection_rolls_up_only_owned_stage_runs() {
        let (delivery, candidate) = current_candidate_delivery();
        let mut snapshot = delivery.into_snapshot();
        let mut task = snapshot.tasks[0].clone();
        task.id = DeliveryTaskId("delivery-task-ui".into());
        task.title = "Invitation UI".into();
        snapshot.tasks.push(task);
        let mut run = snapshot.stage_runs[0].clone();
        run.id = StageRunId("stage-ui".into());
        run.delivery_task_id = Some(DeliveryTaskId("delivery-task-ui".into()));
        run.stage = DeliveryStage::Verifying;
        run.role = "verifier".into();
        run.started_at_millis += 5;
        run.finished_at_millis = run.finished_at_millis.map(|value| value + 5);
        snapshot.stage_runs.push(run);
        let mut binding = snapshot.session_bindings[0].clone();
        binding.id = SessionBindingId("binding-ui".into());
        binding.delivery_task_id = Some(DeliveryTaskId("delivery-task-ui".into()));
        binding.stage_run_id = StageRunId("stage-ui".into());
        binding.product_session_id = ProductSessionId("product-ui".into());
        binding.execution_job_id = ExecutionJobId("job-ui".into());
        binding.worker_session_id = Some(WorkerSessionId("worker-ui".into()));
        binding.codex_thread_id = Some(CodexThreadId("thread-ui".into()));
        binding.bound_at_millis += 5;
        binding = binding.with_test_authority("projection-ui", 1);
        snapshot.session_bindings.push(binding);
        let mut evidence = snapshot.evidence[0].clone();
        evidence.id = EvidenceId("evidence-ui".into());
        evidence.stage_run_id = StageRunId("stage-ui".into());
        evidence.session_binding_id = SessionBindingId("binding-ui".into());
        evidence.source_ref = "runtime-event:thread-ui/1".into();
        evidence.created_at_millis += 5;
        snapshot.evidence.push(evidence);
        let delivery = Delivery::try_from_snapshot(snapshot).expect("two-task Delivery");

        let projection =
            project_delivery_detail(ProjectionInput::new(&delivery).with_candidate(&candidate))
                .expect("task projection");
        let api = projection
            .tasks()
            .iter()
            .find(|task| task.id().0 == "delivery-task-api")
            .expect("API task");
        let ui = projection
            .tasks()
            .iter()
            .find(|task| task.id().0 == "delivery-task-ui")
            .expect("UI task");
        assert_eq!(
            api.stage_run_ids(),
            &[StageRunId("stage-verification-1".into())]
        );
        assert_eq!(ui.stage_run_ids(), &[StageRunId("stage-ui".into())]);
        assert_eq!(api.evidence_refs(), &[EvidenceId("evidence-test-1".into())]);
        assert_eq!(ui.evidence_refs(), &[EvidenceId("evidence-ui".into())]);
    }

    #[test]
    fn attention_projection_uses_current_canonical_items() {
        let mut snapshot = delivery_without_candidate_facts().into_snapshot();
        snapshot.attention_items.push(AttentionItem {
            schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
            id: AttentionItemId("attention-safe".into()),
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            stage_run_id: Some(snapshot.stage_runs[0].id.clone()),
            item_type: AttentionItemType::RequirementQuestion,
            title: "Confirm invitation copy".into(),
            context: r#"{"credential":"must-not-leak","toolPayload":"hidden"}"#.into(),
            options: vec![],
            assigned_to: Some("reviewer".into()),
            blocking: false,
            status: AttentionItemStatus::Resolved,
            resolution: Some(r#"{"authorization":"must-not-leak"}"#.into()),
            resolved_by: Some("reviewer".into()),
            created_at_millis: 1_800_000_000_030,
            resolved_at_millis: Some(1_800_000_000_031),
        });
        let delivery = Delivery::try_from_snapshot(snapshot).expect("Attention Delivery");

        let projection =
            project_delivery_detail(ProjectionInput::new(&delivery)).expect("Attention projection");
        let encoded = String::from_utf8(projection.encode_json().expect("projection JSON"))
            .expect("UTF-8 JSON");
        assert_eq!(
            projection.attention()[0].resolution_summary(),
            Some("resolved")
        );
        assert!(!encoded.contains("must-not-leak"));
        assert!(!encoded.contains("toolPayload"));
        assert!(!encoded.contains("credential"));
        assert!(!encoded.contains("authorization"));
    }

    // Candidate-bound cases use the domain's crate-private sealed fixture
    // builders; no integration caller can create these facts from JSON.
    fn current_candidate_delivery() -> (Delivery, FrozenDeliveryCandidate) {
        let mut snapshot = test_fixture();
        snapshot.stage_runs[0].stage = DeliveryStage::Executing;
        snapshot.stage_runs[0].role = "executor".into();
        let writer = Delivery::try_from_snapshot(snapshot).expect("writer Delivery");
        let candidate = crate::domain::candidate::test_support::frozen_candidate(
            &writer,
            &writer.snapshot().stage_runs[0].id,
            &writer.snapshot().session_bindings[0].id,
        );
        let mut snapshot = writer.into_snapshot();
        for evidence in &mut snapshot.evidence {
            evidence.candidate_ref = candidate.candidate_ref().into();
        }
        let verdict = snapshot.verdict.as_mut().expect("fixture verdict");
        verdict.candidate_ref = candidate.candidate_ref().into();
        for result in &mut verdict.criteria {
            result.candidate_ref = candidate.candidate_ref().into();
        }
        let delivery = Delivery::try_from_snapshot(snapshot).expect("candidate Delivery");
        (delivery, candidate)
    }

    #[test]
    fn evidence_projection_keeps_references_without_log_copies() {
        let (mut delivery, candidate) = current_candidate_delivery();
        let mut snapshot = delivery.into_snapshot();
        let mut stale = snapshot.evidence[0].clone();
        stale.id = EvidenceId("evidence-old-candidate".into());
        stale.candidate_ref = "git-candidate:sha256:old".into();
        stale.source_ref = "runtime-event:old/1".into();
        snapshot.evidence.push(stale);
        delivery =
            Delivery::try_from_snapshot(snapshot).expect("Delivery with historical Evidence");

        let projection =
            project_delivery_detail(ProjectionInput::new(&delivery).with_candidate(&candidate))
                .expect("current Evidence projection");

        assert_eq!(projection.evidence().len(), 1);
        assert_eq!(
            projection.evidence()[0].candidate_ref(),
            candidate.candidate_ref()
        );
        let encoded = String::from_utf8(projection.encode_json().expect("projection JSON"))
            .expect("UTF-8 JSON");
        assert!(encoded.contains("sourceRef"));
        assert!(!encoded.contains("stdout"));
        assert!(!encoded.contains("stderr"));
        assert!(!encoded.contains("rawRuntimeLog"));
    }

    #[test]
    fn verdict_projection_covers_current_acceptance_criteria() {
        let (delivery, candidate) = current_candidate_delivery();
        let expected: Vec<_> = delivery
            .snapshot()
            .spec
            .acceptance_criteria
            .iter()
            .map(|criterion| criterion.id.clone())
            .collect();
        let mut snapshot = delivery.into_snapshot();
        snapshot
            .verdict
            .as_mut()
            .expect("verdict")
            .criteria
            .reverse();
        let delivery = Delivery::try_from_snapshot(snapshot).expect("reordered verdict");

        let projection =
            project_delivery_detail(ProjectionInput::new(&delivery).with_candidate(&candidate))
                .expect("verdict projection");
        let actual: Vec<_> = projection
            .verdict()
            .expect("current verdict")
            .criteria()
            .iter()
            .map(|criterion| criterion.criterion_id().clone())
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn stale_candidate_verdict_is_rejected_instead_of_displayed() {
        let (delivery, candidate) = current_candidate_delivery();
        let mut snapshot = delivery.into_snapshot();
        let stale_ref = "git-candidate:sha256:historical";
        for evidence in &mut snapshot.evidence {
            evidence.candidate_ref = stale_ref.into();
        }
        let verdict = snapshot.verdict.as_mut().expect("verdict");
        verdict.candidate_ref = stale_ref.into();
        for result in &mut verdict.criteria {
            result.candidate_ref = stale_ref.into();
        }
        let delivery = Delivery::try_from_snapshot(snapshot).expect("historical verdict Delivery");

        assert_eq!(
            project_delivery_detail(ProjectionInput::new(&delivery).with_candidate(&candidate))
                .expect_err("historical verdict")
                .code(),
            ProjectionErrorCode::InconsistentCurrentVerdict
        );
        assert_eq!(
            project_delivery_detail(ProjectionInput::new(&delivery))
                .expect_err("missing sealed candidate")
                .code(),
            ProjectionErrorCode::MissingCurrentCandidate
        );
    }
}
