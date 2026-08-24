// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{DeliveryId, EvidenceId};

use super::evidence::{ResolvedDeliveryEvidence, VerifiedEvidenceOutcome};
use super::rework::{VerdictAttentionAction, safest_attention_transition};
use super::verification::{
    IndependentVerification, VerificationFindingConclusion, VerificationJobOutcomeStatus,
    VerificationRole, VerificationRoleSettlement, VerificationSessionState,
    VerificationTerminalSettlement,
};
use super::{
    AcceptanceCriterion, AcceptanceCriterionId, AttentionItemStatus, AttentionItemType,
    CriterionResultId, Delivery, DeliverySpecId, DeliveryStatus, DeliveryTaskStatus,
    DeliveryValidationError, DeliveryValidationErrorCode, DeliveryVerdictId, EvidenceRef,
    EvidenceRefType, FrozenDeliveryCandidate, MAX_REFERENCE_LENGTH, MAX_TEXT_LENGTH,
    StageRunActorType, StageRunStatus, assert_frozen_candidate_current, bounded_text,
    collection_length, duplicate_ids, portable_identifier, safe_non_negative, schema_version,
    unique_texts, validation_error,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CriterionVerdict {
    #[serde(rename = "pass")]
    Pass,
    #[serde(rename = "fail")]
    Fail,
    #[serde(rename = "inconclusive")]
    Inconclusive,
    #[serde(rename = "infra_error")]
    InfraError,
}

pub type DeliveryVerdictStatus = CriterionVerdict;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CriterionResult {
    pub schema_version: u8,
    pub id: CriterionResultId,
    pub delivery_id: DeliveryId,
    pub delivery_spec_id: DeliverySpecId,
    pub criterion_id: AcceptanceCriterionId,
    pub candidate_ref: String,
    pub verdict: CriterionVerdict,
    pub evidence_refs: Vec<EvidenceId>,
    pub explanation: String,
    pub evaluated_at_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryVerdict {
    pub schema_version: u8,
    pub id: DeliveryVerdictId,
    pub delivery_id: DeliveryId,
    pub delivery_spec_id: DeliverySpecId,
    pub candidate_ref: String,
    pub status: DeliveryVerdictStatus,
    pub criteria: Vec<CriterionResult>,
    pub unresolved_findings: Vec<String>,
    pub produced_at_millis: u64,
}

/// Evidence and verdict facts derived together for one atomic Delivery write.
///
/// Its fields are private and there is no deserializer or public constructor.
/// Callers can inspect or move the computed canonical facts, but cannot submit
/// a preselected status, `CriterionResult`, `DeliveryVerdict`, or Attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedDeliveryVerdict {
    evidence: Vec<EvidenceRef>,
    verdict: DeliveryVerdict,
    next_status: DeliveryStatus,
}

impl ComputedDeliveryVerdict {
    pub fn evidence(&self) -> &[EvidenceRef] {
        &self.evidence
    }

    pub fn verdict(&self) -> &DeliveryVerdict {
        &self.verdict
    }

    pub const fn next_status(&self) -> DeliveryStatus {
        self.next_status
    }

    pub(crate) fn into_parts(self) -> (Vec<EvidenceRef>, DeliveryVerdict, DeliveryStatus) {
        (self.evidence, self.verdict, self.next_status)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProductOutcome {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoleOutcome {
    Product(ProductOutcome),
    Inconclusive,
    InfraError,
}

#[derive(Debug)]
struct RoleEvaluation {
    role: VerificationRole,
    outcome: RoleOutcome,
    evidence_ids: Vec<EvidenceId>,
    explanation: String,
    unresolved_finding: Option<String>,
    action: Option<VerdictAttentionAction>,
}

#[derive(Debug)]
struct CriterionComputation {
    verdict: CriterionVerdict,
    evidence_ids: Vec<EvidenceId>,
    explanation: String,
    unresolved_findings: Vec<String>,
    actions: Vec<VerdictAttentionAction>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CriterionResultIdentity<'result> {
    delivery_spec_id: &'result DeliverySpecId,
    delivery_spec_revision: u64,
    candidate_ref: &'result str,
    criterion_id: &'result AcceptanceCriterionId,
    verdict: CriterionVerdict,
    evidence_refs: &'result [EvidenceId],
    explanation: &'result str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryVerdictCriterionIdentity<'criterion> {
    id: &'criterion CriterionResultId,
    criterion_id: &'criterion AcceptanceCriterionId,
    verdict: CriterionVerdict,
    evidence_refs: &'criterion [EvidenceId],
    explanation: &'criterion str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryVerdictIdentity<'verdict> {
    delivery_spec_id: &'verdict DeliverySpecId,
    delivery_spec_revision: u64,
    candidate_ref: &'verdict str,
    status: DeliveryVerdictStatus,
    criteria: &'verdict [DeliveryVerdictCriterionIdentity<'verdict>],
    unresolved_findings: &'verdict [String],
}

/// Computes one deterministic, fail-closed verdict from sealed domain facts.
///
/// This is the module's only computation entry. It never accepts caller-made
/// Evidence, criterion results, verdict status, or Attention values.
///
/// # Errors
///
/// Rejects stale candidates or verification projections, duplicate/foreign
/// Evidence, invalid time ordering, and any derived facts that do not satisfy
/// the canonical Delivery relationships.
#[allow(clippy::too_many_lines)]
pub fn compute_delivery_verdict(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    verification: &IndependentVerification,
    evidence: &[ResolvedDeliveryEvidence],
    produced_at_millis: u64,
) -> Result<ComputedDeliveryVerdict, DeliveryValidationError> {
    assert_frozen_candidate_current(delivery, candidate)?;
    if verification.candidate_ref() != candidate.candidate_ref() {
        return Err(invalid_computation(
            "verification belongs to another frozen candidate",
        ));
    }
    validate_verification_is_current(delivery, candidate, verification)?;
    safe_non_negative(produced_at_millis, "verdict.producedAtMillis")?;
    if produced_at_millis < delivery.snapshot().updated_at_millis {
        return Err(invalid_computation(
            "verdict production time precedes the current Delivery",
        ));
    }
    validate_resolved_evidence(delivery, candidate, evidence, produced_at_millis)?;
    validate_verification_time(verification, produced_at_millis)?;

    let mut actions = Vec::new();
    let mut unresolved_findings = Vec::new();
    for item in &delivery.snapshot().attention_items {
        if item.blocking
            && item.status == AttentionItemStatus::Open
            && item.item_type != AttentionItemType::DeliveryApproval
        {
            unresolved_findings.push(format!("blocking-attention:{}", item.id.0));
            let action = match item.item_type {
                AttentionItemType::VerificationBlocked => VerdictAttentionAction::RetryVerification,
                AttentionItemType::RequirementQuestion
                | AttentionItemType::DecisionRequired
                | AttentionItemType::ScopeChange => VerdictAttentionAction::ClarifyDefinition,
                AttentionItemType::DeliveryApproval => continue,
            };
            actions.push(action);
        }
    }

    let mut criteria = Vec::with_capacity(delivery.snapshot().spec.acceptance_criteria.len());
    let mut used_evidence_ids = HashSet::new();
    for criterion in &delivery.snapshot().spec.acceptance_criteria {
        let computed = compute_criterion(criterion, verification, evidence);
        for id in &computed.evidence_ids {
            used_evidence_ids.insert(id.0.clone());
        }
        if criterion.required {
            unresolved_findings.extend(computed.unresolved_findings.iter().cloned());
            actions.extend(computed.actions.iter().copied());
        }
        let id = deterministic_id(
            "criterion-result:sha256",
            &CriterionResultIdentity {
                delivery_spec_id: &delivery.snapshot().spec.id,
                delivery_spec_revision: delivery.snapshot().spec.revision,
                candidate_ref: candidate.candidate_ref(),
                criterion_id: &criterion.id,
                verdict: computed.verdict,
                evidence_refs: &computed.evidence_ids,
                explanation: &computed.explanation,
            },
        )?;
        criteria.push(CriterionResult {
            schema_version: super::DELIVERY_SCHEMA_VERSION,
            id: CriterionResultId(id),
            delivery_id: delivery.id().clone(),
            delivery_spec_id: delivery.snapshot().spec.id.clone(),
            criterion_id: criterion.id.clone(),
            candidate_ref: candidate.candidate_ref().into(),
            verdict: computed.verdict,
            evidence_refs: computed.evidence_ids,
            explanation: computed.explanation,
            evaluated_at_millis: produced_at_millis,
        });
    }

    unresolved_findings.sort();
    unresolved_findings.dedup();
    let status = fold_delivery_status(delivery, &criteria, &unresolved_findings);
    if status == DeliveryVerdictStatus::Fail {
        let attempts = delivery
            .snapshot()
            .stage_runs
            .iter()
            .filter(|run| run.stage == super::DeliveryStage::Reworking)
            .count() as u64;
        if attempts >= delivery.snapshot().spec.max_rework_attempts {
            actions.push(VerdictAttentionAction::ClarifyDefinition);
        } else {
            actions.push(VerdictAttentionAction::StartRework);
        }
    }

    let verdict_criterion_identities = criteria
        .iter()
        .map(|criterion| DeliveryVerdictCriterionIdentity {
            id: &criterion.id,
            criterion_id: &criterion.criterion_id,
            verdict: criterion.verdict,
            evidence_refs: &criterion.evidence_refs,
            explanation: &criterion.explanation,
        })
        .collect::<Vec<_>>();
    let id = deterministic_id(
        "delivery-verdict:sha256",
        &DeliveryVerdictIdentity {
            delivery_spec_id: &delivery.snapshot().spec.id,
            delivery_spec_revision: delivery.snapshot().spec.revision,
            candidate_ref: candidate.candidate_ref(),
            status,
            criteria: &verdict_criterion_identities,
            unresolved_findings: &unresolved_findings,
        },
    )?;
    let verdict = DeliveryVerdict {
        schema_version: super::DELIVERY_SCHEMA_VERSION,
        id: DeliveryVerdictId(id),
        delivery_id: delivery.id().clone(),
        delivery_spec_id: delivery.snapshot().spec.id.clone(),
        candidate_ref: candidate.candidate_ref().into(),
        status,
        criteria,
        unresolved_findings,
        produced_at_millis,
    };

    let mut canonical_evidence = evidence
        .iter()
        .filter(|resolved| used_evidence_ids.contains(&resolved.evidence().id.0))
        .map(|resolved| resolved.evidence().clone())
        .collect::<Vec<_>>();
    canonical_evidence.sort_by(|left, right| left.id.0.cmp(&right.id.0));

    validate_computed_facts(delivery, &canonical_evidence, &verdict, produced_at_millis)?;
    let all_tasks_complete = delivery
        .snapshot()
        .tasks
        .iter()
        .all(|task| task.status == DeliveryTaskStatus::Completed);
    let next_status = if status == DeliveryVerdictStatus::Pass && all_tasks_complete {
        DeliveryStatus::ReadyToDeliver
    } else if actions.is_empty() {
        DeliveryStatus::Verifying
    } else {
        safest_attention_transition(&actions)
    };
    Ok(ComputedDeliveryVerdict {
        evidence: canonical_evidence,
        verdict,
        next_status,
    })
}

fn validate_verification_is_current(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    verification: &IndependentVerification,
) -> Result<(), DeliveryValidationError> {
    for settlement in verification.settlements() {
        let Some(assignment) = settlement.assignment() else {
            continue;
        };
        let role_runs = delivery
            .snapshot()
            .stage_runs
            .iter()
            .filter(|run| {
                run.stage == super::DeliveryStage::Verifying
                    && run.actor_type == StageRunActorType::Codex
                    && run.role == role_id(settlement.role())
            })
            .collect::<Vec<_>>();
        let current_attempt = role_runs.iter().map(|run| run.attempt).max();
        let current_runs = role_runs
            .into_iter()
            .filter(|run| Some(run.attempt) == current_attempt)
            .collect::<Vec<_>>();
        if current_runs.len() != 1 || current_runs[0].id != *assignment.stage_run_id() {
            return Err(invalid_computation(
                "verification projection no longer owns the role's current StageRun",
            ));
        }
        let current_bindings = delivery
            .snapshot()
            .session_bindings
            .iter()
            .filter(|binding| binding.stage_run_id == current_runs[0].id)
            .collect::<Vec<_>>();
        if current_bindings.len() != 1 {
            return Err(invalid_computation(
                "verification projection no longer has one current SessionBinding",
            ));
        }
        let binding = current_bindings[0];
        let binding_is_current = binding.id == *assignment.session_binding_id()
            && binding.product_session_id == *assignment.product_session_id()
            && binding.execution_job_id == *assignment.execution_job_id()
            && binding.worker_session_id.as_ref() == Some(assignment.worker_session_id())
            && binding.codex_thread_id.as_ref() == Some(assignment.codex_thread_id())
            && assignment.repository() == candidate.repository()
            && assignment.checkout_revision() == candidate.candidate_commit_id()
            && current_runs[0].delivery_task_id.as_ref() == candidate.producer_delivery_task_id();
        if !binding_is_current {
            return Err(invalid_computation(
                "verification projection Session or candidate checkout is stale",
            ));
        }
        if !terminal_stage_is_current(settlement, current_runs[0], binding) {
            return Err(invalid_computation(
                "verification projection terminal StageRun facts are stale",
            ));
        }
    }
    Ok(())
}

fn terminal_stage_is_current(
    settlement: &VerificationRoleSettlement,
    run: &super::StageRun,
    binding: &super::SessionBinding,
) -> bool {
    let Some(terminal) = settlement.terminal_job_outcome() else {
        return match settlement.state() {
            VerificationSessionState::Running => matches!(
                run.status,
                StageRunStatus::Running | StageRunStatus::Waiting
            ),
            VerificationSessionState::Incomplete => !matches!(
                run.status,
                StageRunStatus::Running | StageRunStatus::Waiting
            ),
            VerificationSessionState::Missing => true,
            VerificationSessionState::Failed
            | VerificationSessionState::Cancelled
            | VerificationSessionState::Settled => false,
        };
    };
    let identity_is_current = terminal.stage_run_id() == &run.id
        && terminal.role_id() == run.role
        && terminal.execution_job_id() == &binding.execution_job_id
        && terminal.product_session_id() == &binding.product_session_id
        && terminal.attempt() == run.attempt
        && binding.worker_session_id.as_ref() == Some(terminal.worker_session_id())
        && binding.codex_thread_id.as_ref() == Some(terminal.codex_thread_id())
        && run.finished_at_millis == Some(terminal.finished_at_millis())
        && binding.bound_at_millis <= terminal.finished_at_millis();
    let status_is_current = matches!(
        (
            settlement.state(),
            settlement.terminal_settlement(),
            run.status,
            terminal.status(),
        ),
        (
            VerificationSessionState::Settled | VerificationSessionState::Incomplete,
            Some(VerificationTerminalSettlement::Settled),
            StageRunStatus::Succeeded,
            VerificationJobOutcomeStatus::Succeeded,
        ) | (
            VerificationSessionState::Failed,
            Some(VerificationTerminalSettlement::Failed),
            StageRunStatus::Failed,
            VerificationJobOutcomeStatus::Failed,
        ) | (
            VerificationSessionState::Failed,
            Some(VerificationTerminalSettlement::InfrastructureError),
            StageRunStatus::Failed,
            VerificationJobOutcomeStatus::InfrastructureError,
        ) | (
            VerificationSessionState::Cancelled,
            Some(VerificationTerminalSettlement::Cancelled),
            StageRunStatus::Cancelled,
            VerificationJobOutcomeStatus::Cancelled,
        )
    );
    identity_is_current && status_is_current
}

fn validate_resolved_evidence(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    evidence: &[ResolvedDeliveryEvidence],
    produced_at_millis: u64,
) -> Result<(), DeliveryValidationError> {
    collection_length(evidence.len(), "verdict.evidence")?;
    let mut ids = HashSet::with_capacity(evidence.len());
    for resolved in evidence {
        let reference = resolved.evidence();
        let current = reference.delivery_id == *delivery.id()
            && reference.delivery_spec_id == delivery.snapshot().spec.id
            && reference.delivery_spec_revision == delivery.snapshot().spec.revision
            && reference.candidate_ref == candidate.candidate_ref()
            && reference.created_at_millis <= produced_at_millis;
        if !current || !ids.insert(reference.id.0.as_str()) {
            return Err(invalid_computation(
                "resolved Evidence is duplicate, later, stale, or foreign",
            ));
        }
    }
    Ok(())
}

fn validate_verification_time(
    verification: &IndependentVerification,
    produced_at_millis: u64,
) -> Result<(), DeliveryValidationError> {
    for settlement in verification.settlements() {
        if settlement
            .terminal_job_outcome()
            .is_some_and(|outcome| outcome.finished_at_millis() > produced_at_millis)
        {
            return Err(invalid_computation(
                "verdict production time precedes a verification terminal outcome",
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn compute_criterion(
    criterion: &AcceptanceCriterion,
    verification: &IndependentVerification,
    evidence: &[ResolvedDeliveryEvidence],
) -> CriterionComputation {
    if criterion.verification_method.is_none() {
        return CriterionComputation {
            verdict: CriterionVerdict::Inconclusive,
            evidence_ids: vec![],
            explanation: format!(
                "{}: approved verification method is missing",
                criterion.id.0
            ),
            unresolved_findings: vec![format!(
                "verification-blocked:{}:missing-method",
                criterion.id.0
            )],
            actions: vec![VerdictAttentionAction::ClarifyDefinition],
        };
    }

    let mut roles = verification
        .settlements()
        .iter()
        .map(|settlement| evaluate_role(criterion, settlement, evidence))
        .collect::<Vec<_>>();
    roles.sort_by_key(|evaluation| role_order(evaluation.role));
    let mut evidence_ids = roles
        .iter()
        .flat_map(|evaluation| evaluation.evidence_ids.iter().cloned())
        .collect::<Vec<_>>();
    evidence_ids.sort_by(|left, right| left.0.cmp(&right.0));
    evidence_ids.dedup();
    let mut unresolved_findings = roles
        .iter()
        .filter_map(|evaluation| evaluation.unresolved_finding.clone())
        .collect::<Vec<_>>();
    let mut actions = roles
        .iter()
        .filter_map(|evaluation| evaluation.action)
        .collect::<Vec<_>>();

    let infra = roles
        .iter()
        .any(|evaluation| evaluation.outcome == RoleOutcome::InfraError);
    let products = roles
        .iter()
        .filter_map(|evaluation| match evaluation.outcome {
            RoleOutcome::Product(outcome) => Some(outcome),
            RoleOutcome::Inconclusive | RoleOutcome::InfraError => None,
        })
        .collect::<Vec<_>>();
    let every_role_has_product = products.len() == roles.len() && !roles.is_empty();
    let unanimous = products
        .first()
        .is_some_and(|first| products.iter().all(|outcome| outcome == first));
    let verdict = if infra {
        CriterionVerdict::InfraError
    } else if every_role_has_product && unanimous {
        match products[0] {
            ProductOutcome::Pass => CriterionVerdict::Pass,
            ProductOutcome::Fail => CriterionVerdict::Fail,
        }
    } else {
        if every_role_has_product && !unanimous {
            let conclusions = roles
                .iter()
                .map(|evaluation| {
                    format!(
                        "{}={}",
                        role_id(evaluation.role),
                        role_outcome_name(evaluation.outcome)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            unresolved_findings.push(format!("contradiction:{}:{conclusions}", criterion.id.0));
            actions.push(VerdictAttentionAction::ClarifyDefinition);
        }
        CriterionVerdict::Inconclusive
    };
    if matches!(verdict, CriterionVerdict::Pass | CriterionVerdict::Fail) && evidence_ids.is_empty()
    {
        unresolved_findings.push(format!(
            "evidence-mismatch:{}:no-direct-evidence",
            criterion.id.0
        ));
        actions.push(VerdictAttentionAction::RetryVerification);
        return CriterionComputation {
            verdict: CriterionVerdict::Inconclusive,
            evidence_ids,
            explanation: roles
                .iter()
                .map(|evaluation| evaluation.explanation.as_str())
                .collect::<Vec<_>>()
                .join(" | "),
            unresolved_findings,
            actions,
        };
    }
    CriterionComputation {
        verdict,
        evidence_ids,
        explanation: roles
            .iter()
            .map(|evaluation| evaluation.explanation.as_str())
            .collect::<Vec<_>>()
            .join(" | "),
        unresolved_findings,
        actions,
    }
}

fn evaluate_role(
    criterion: &AcceptanceCriterion,
    settlement: &VerificationRoleSettlement,
    evidence: &[ResolvedDeliveryEvidence],
) -> RoleEvaluation {
    let role = settlement.role();
    match settlement.state() {
        VerificationSessionState::Missing => role_inconclusive(
            role,
            &criterion.id,
            "required verification Session is missing",
            "missing-session",
            VerdictAttentionAction::RetryVerification,
        ),
        VerificationSessionState::Running | VerificationSessionState::Incomplete => {
            role_inconclusive(
                role,
                &criterion.id,
                "verification Session has no complete accepted result",
                "incomplete-session",
                VerdictAttentionAction::RetryVerification,
            )
        }
        VerificationSessionState::Failed | VerificationSessionState::Cancelled => RoleEvaluation {
            role,
            outcome: RoleOutcome::InfraError,
            evidence_ids: vec![],
            explanation: format!(
                "{}: verification Session ended with an environment failure",
                role_id(role)
            ),
            unresolved_finding: None,
            action: Some(VerdictAttentionAction::RetryVerification),
        },
        VerificationSessionState::Settled => evaluate_settled_role(criterion, settlement, evidence),
    }
}

#[allow(clippy::too_many_lines)]
fn evaluate_settled_role(
    criterion: &AcceptanceCriterion,
    settlement: &VerificationRoleSettlement,
    evidence: &[ResolvedDeliveryEvidence],
) -> RoleEvaluation {
    let role = settlement.role();
    if settlement.terminal_settlement() != Some(VerificationTerminalSettlement::Settled) {
        return RoleEvaluation {
            role,
            outcome: RoleOutcome::InfraError,
            evidence_ids: vec![],
            explanation: format!("{}: terminal verification failed", role_id(role)),
            unresolved_finding: None,
            action: Some(VerdictAttentionAction::RetryVerification),
        };
    }
    let Some(assignment) = settlement.assignment() else {
        return role_inconclusive(
            role,
            &criterion.id,
            "settled verification has no current assignment",
            "missing-assignment",
            VerdictAttentionAction::RetryVerification,
        );
    };
    let Some(terminal) = settlement.terminal_job_outcome() else {
        return role_inconclusive(
            role,
            &criterion.id,
            "settled verification has no accepted terminal Worker outcome",
            "missing-terminal-outcome",
            VerdictAttentionAction::RetryVerification,
        );
    };
    let findings = settlement
        .findings()
        .iter()
        .filter(|finding| finding.criterion_id() == &criterion.id)
        .collect::<Vec<_>>();
    if findings.len() != 1 {
        return role_inconclusive(
            role,
            &criterion.id,
            "verification must produce exactly one current finding",
            "missing-or-ambiguous-finding",
            VerdictAttentionAction::RetryVerification,
        );
    }
    let finding = findings[0];
    if finding.source_refs().len() != finding.source_sequences().len() {
        return role_evidence_mismatch(
            role,
            &criterion.id,
            finding.finding_ref(),
            "finding source references and source positions differ in length",
            vec![],
        );
    }
    let mut resolved_sources = Vec::with_capacity(finding.source_refs().len());
    for (source_ref, source_sequence) in
        finding.source_refs().iter().zip(finding.source_sequences())
    {
        let matching = evidence
            .iter()
            .filter(|resolved| {
                let reference = resolved.evidence();
                reference.source_ref == *source_ref
                    && reference.stage_run_id == *assignment.stage_run_id()
                    && reference.session_binding_id == *assignment.session_binding_id()
                    && reference.candidate_ref == finding.candidate_ref()
            })
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return role_evidence_mismatch(
                role,
                &criterion.id,
                finding.finding_ref(),
                "finding source does not resolve to one exact current EvidenceRef",
                source_evidence_ids(&resolved_sources),
            );
        }
        resolved_sources.push(matching[0]);
        let Ok(source_sequence) = u64::try_from(source_sequence.0) else {
            return role_evidence_mismatch(
                role,
                &criterion.id,
                finding.finding_ref(),
                "finding source position is outside the accepted runtime sequence range",
                source_evidence_ids(&resolved_sources),
            );
        };
        if !matching[0].matches_finding_source(terminal, source_sequence) {
            return role_evidence_mismatch(
                role,
                &criterion.id,
                finding.finding_ref(),
                "Evidence source position or terminal Worker lease differs from the finding",
                source_evidence_ids(&resolved_sources),
            );
        }
    }
    if resolved_sources.iter().any(|resolved| {
        matches!(
            resolved.outcome(),
            VerifiedEvidenceOutcome::TimedOut
                | VerifiedEvidenceOutcome::PolicyDenied
                | VerifiedEvidenceOutcome::InfrastructureFailed
                | VerifiedEvidenceOutcome::Cancelled
        )
    }) {
        return RoleEvaluation {
            role,
            outcome: RoleOutcome::InfraError,
            evidence_ids: source_evidence_ids(&resolved_sources),
            explanation: format!(
                "{}: direct supporting execution had an environment failure",
                role_id(role)
            ),
            unresolved_finding: None,
            action: Some(VerdictAttentionAction::RetryVerification),
        };
    }

    let direct = resolved_sources
        .iter()
        .copied()
        .filter(|resolved| direct_evidence_type(resolved.evidence().evidence_type))
        .collect::<Vec<_>>();
    if direct.is_empty() {
        return role_evidence_mismatch(
            role,
            &criterion.id,
            finding.finding_ref(),
            "Agent message, review finding, or generic runtime event is not direct evidence",
            source_evidence_ids(&resolved_sources),
        );
    }
    let claimed = match finding.conclusion() {
        VerificationFindingConclusion::Pass => ProductOutcome::Pass,
        VerificationFindingConclusion::Fail => ProductOutcome::Fail,
    };
    let mut observed = Vec::with_capacity(direct.len());
    for resolved in &direct {
        let outcome = match resolved.outcome() {
            VerifiedEvidenceOutcome::Succeeded => Some(ProductOutcome::Pass),
            VerifiedEvidenceOutcome::Failed => Some(ProductOutcome::Fail),
            VerifiedEvidenceOutcome::Observed
                if matches!(
                    resolved.evidence().evidence_type,
                    EvidenceRefType::Diff | EvidenceRefType::File | EvidenceRefType::Commit
                ) =>
            {
                Some(claimed)
            }
            VerifiedEvidenceOutcome::Observed
            | VerifiedEvidenceOutcome::TimedOut
            | VerifiedEvidenceOutcome::PolicyDenied
            | VerifiedEvidenceOutcome::InfrastructureFailed
            | VerifiedEvidenceOutcome::Cancelled => None,
        };
        let Some(outcome) = outcome else {
            return role_evidence_mismatch(
                role,
                &criterion.id,
                finding.finding_ref(),
                "test or command has no successful or failed direct outcome",
                source_evidence_ids(&direct),
            );
        };
        observed.push(outcome);
    }
    if observed.iter().any(|outcome| *outcome != claimed)
        || observed
            .first()
            .is_some_and(|first| observed.iter().any(|outcome| outcome != first))
    {
        return role_evidence_mismatch(
            role,
            &criterion.id,
            finding.finding_ref(),
            "Agent conclusion conflicts with the direct evidence outcome",
            source_evidence_ids(&direct),
        );
    }
    RoleEvaluation {
        role,
        outcome: RoleOutcome::Product(claimed),
        evidence_ids: source_evidence_ids(&direct),
        explanation: format!(
            "{}: {} is supported by current direct Evidence",
            role_id(role),
            product_outcome_name(claimed)
        ),
        unresolved_finding: None,
        action: None,
    }
}

fn source_evidence_ids(evidence: &[&ResolvedDeliveryEvidence]) -> Vec<EvidenceId> {
    let mut ids = evidence
        .iter()
        .map(|resolved| resolved.evidence().id.clone())
        .collect::<Vec<_>>();
    ids.sort_by(|left, right| left.0.cmp(&right.0));
    ids.dedup();
    ids
}

fn role_inconclusive(
    role: VerificationRole,
    criterion_id: &AcceptanceCriterionId,
    explanation: &str,
    reason: &str,
    action: VerdictAttentionAction,
) -> RoleEvaluation {
    RoleEvaluation {
        role,
        outcome: RoleOutcome::Inconclusive,
        evidence_ids: vec![],
        explanation: format!("{}: {explanation}", role_id(role)),
        unresolved_finding: Some(format!(
            "verification-inconclusive:{}:{}:{reason}",
            criterion_id.0,
            role_id(role)
        )),
        action: Some(action),
    }
}

fn role_evidence_mismatch(
    role: VerificationRole,
    criterion_id: &AcceptanceCriterionId,
    finding_ref: &str,
    explanation: &str,
    evidence_ids: Vec<EvidenceId>,
) -> RoleEvaluation {
    RoleEvaluation {
        role,
        outcome: RoleOutcome::Inconclusive,
        evidence_ids,
        explanation: format!("{}: {explanation}", role_id(role)),
        unresolved_finding: Some(format!(
            "evidence-mismatch:{}:{}:{finding_ref}",
            criterion_id.0,
            role_id(role)
        )),
        action: Some(VerdictAttentionAction::ClarifyDefinition),
    }
}

const fn direct_evidence_type(evidence_type: EvidenceRefType) -> bool {
    matches!(
        evidence_type,
        EvidenceRefType::Test
            | EvidenceRefType::Command
            | EvidenceRefType::Diff
            | EvidenceRefType::File
            | EvidenceRefType::Commit
    )
}

const fn role_order(role: VerificationRole) -> u8 {
    match role {
        VerificationRole::Reviewer => 0,
        VerificationRole::Verifier => 1,
        VerificationRole::AdversarialVerifier => 2,
    }
}

const fn role_id(role: VerificationRole) -> &'static str {
    match role {
        VerificationRole::Reviewer => "reviewer",
        VerificationRole::Verifier => "verifier",
        VerificationRole::AdversarialVerifier => "adversarial-verifier",
    }
}

const fn product_outcome_name(outcome: ProductOutcome) -> &'static str {
    match outcome {
        ProductOutcome::Pass => "pass",
        ProductOutcome::Fail => "fail",
    }
}

const fn role_outcome_name(outcome: RoleOutcome) -> &'static str {
    match outcome {
        RoleOutcome::Product(product) => product_outcome_name(product),
        RoleOutcome::Inconclusive => "inconclusive",
        RoleOutcome::InfraError => "infra_error",
    }
}

fn fold_delivery_status(
    delivery: &Delivery,
    criteria: &[CriterionResult],
    unresolved_findings: &[String],
) -> DeliveryVerdictStatus {
    let required_ids = delivery
        .snapshot()
        .spec
        .acceptance_criteria
        .iter()
        .filter(|criterion| criterion.required)
        .map(|criterion| criterion.id.0.as_str())
        .collect::<HashSet<_>>();
    let required = criteria
        .iter()
        .filter(|result| required_ids.contains(result.criterion_id.0.as_str()))
        .collect::<Vec<_>>();
    if required
        .iter()
        .any(|result| result.verdict == CriterionVerdict::Fail)
    {
        CriterionVerdict::Fail
    } else if required
        .iter()
        .any(|result| result.verdict == CriterionVerdict::InfraError)
    {
        CriterionVerdict::InfraError
    } else if required
        .iter()
        .any(|result| result.verdict == CriterionVerdict::Inconclusive)
        || !unresolved_findings.is_empty()
    {
        CriterionVerdict::Inconclusive
    } else {
        CriterionVerdict::Pass
    }
}

fn deterministic_id(
    prefix: &str,
    identity: &impl Serialize,
) -> Result<String, DeliveryValidationError> {
    let encoded = serde_json::to_vec(identity).map_err(|error| {
        validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            "verdict",
            format!("derived verdict identity cannot be encoded: {error}"),
        )
    })?;
    Ok(format!("{prefix}:{:x}", Sha256::digest(encoded)))
}

fn validate_computed_facts(
    delivery: &Delivery,
    evidence: &[EvidenceRef],
    verdict: &DeliveryVerdict,
    produced_at_millis: u64,
) -> Result<(), DeliveryValidationError> {
    let mut trial = delivery.snapshot().clone();
    trial.status = DeliveryStatus::Verifying;
    trial.evidence = evidence.to_vec();
    trial.verdict = Some(verdict.clone());
    trial.updated_at_millis = produced_at_millis;
    Delivery::try_from_snapshot(trial).map(|_| ())
}

fn invalid_computation(message: &str) -> DeliveryValidationError {
    validation_error(
        DeliveryValidationErrorCode::RelationshipMismatch,
        "verdict",
        message,
    )
}

pub(crate) fn validate(
    verdict: &DeliveryVerdict,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    schema_version(verdict.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&verdict.id.0, &format!("{path}.id"))?;
    portable_identifier(&verdict.delivery_id.0, &format!("{path}.deliveryId"))?;
    portable_identifier(
        &verdict.delivery_spec_id.0,
        &format!("{path}.deliverySpecId"),
    )?;
    bounded_text(
        &verdict.candidate_ref,
        &format!("{path}.candidateRef"),
        MAX_REFERENCE_LENGTH,
    )?;
    collection_length(verdict.criteria.len(), &format!("{path}.criteria"))?;
    if verdict.criteria.is_empty() {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidVerdict,
            format!("{path}.criteria"),
            "delivery verdict must evaluate criteria",
        ));
    }
    for (index, result) in verdict.criteria.iter().enumerate() {
        validate_result(result, &format!("{path}.criteria[{index}]"))?;
    }
    duplicate_ids(
        verdict.criteria.iter().map(|result| result.id.0.as_str()),
        &format!("{path}.criteria"),
    )?;
    duplicate_ids(
        verdict
            .criteria
            .iter()
            .map(|result| result.criterion_id.0.as_str()),
        &format!("{path}.criteria"),
    )?;
    unique_texts(
        &verdict.unresolved_findings,
        &format!("{path}.unresolvedFindings"),
    )?;
    safe_non_negative(
        verdict.produced_at_millis,
        &format!("{path}.producedAtMillis"),
    )
}

fn validate_result(result: &CriterionResult, path: &str) -> Result<(), DeliveryValidationError> {
    schema_version(result.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&result.id.0, &format!("{path}.id"))?;
    portable_identifier(&result.delivery_id.0, &format!("{path}.deliveryId"))?;
    portable_identifier(
        &result.delivery_spec_id.0,
        &format!("{path}.deliverySpecId"),
    )?;
    portable_identifier(&result.criterion_id.0, &format!("{path}.criterionId"))?;
    bounded_text(
        &result.candidate_ref,
        &format!("{path}.candidateRef"),
        MAX_REFERENCE_LENGTH,
    )?;
    collection_length(result.evidence_refs.len(), &format!("{path}.evidenceRefs"))?;
    for (index, evidence_id) in result.evidence_refs.iter().enumerate() {
        portable_identifier(&evidence_id.0, &format!("{path}.evidenceRefs[{index}]"))?;
    }
    duplicate_ids(
        result
            .evidence_refs
            .iter()
            .map(|evidence_id| evidence_id.0.as_str()),
        &format!("{path}.evidenceRefs"),
    )?;
    if matches!(
        result.verdict,
        CriterionVerdict::Pass | CriterionVerdict::Fail
    ) && result.evidence_refs.is_empty()
    {
        return Err(validation_error(
            DeliveryValidationErrorCode::InvalidVerdict,
            format!("{path}.evidenceRefs"),
            "pass or fail criterion result must cite evidence",
        ));
    }
    bounded_text(
        &result.explanation,
        &format!("{path}.explanation"),
        MAX_TEXT_LENGTH,
    )?;
    safe_non_negative(
        result.evaluated_at_millis,
        &format!("{path}.evaluatedAtMillis"),
    )
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, ProductSessionId, StageRunId, WorkerSessionId,
    };

    use super::compute_delivery_verdict;
    use crate::domain::candidate::test_support::frozen_candidate;
    use crate::domain::evidence::{
        EvidenceRefType, VerifiedEvidenceOutcome,
        test_support::{resolved_role_evidence, resolved_role_evidence_at_sequence},
    };
    use crate::domain::verification::{
        VerificationFacts, VerificationPermissionProfile, VerificationRole,
        VerificationSessionFacts, VerificationSessionState, VerificationWorkspaceMode,
        test_support::{VerificationFixtureState, fixture_evidence_id, independent_verification},
        validate_independent_verification,
    };
    use crate::domain::{
        CriterionVerdict, Delivery, DeliveryStage, DeliveryStatus, DeliveryTaskStatus,
        FrozenDeliveryCandidate, SessionBinding, SessionBindingId, StageRun, StageRunActorType,
        StageRunStatus, test_fixture,
    };

    const PRODUCED_AT_MILLIS: u64 = 1_800_000_000_100;

    fn verdict_delivery() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;

        let task_id = {
            let writer = &mut snapshot.stage_runs[0];
            writer.id = StageRunId("stage-executor-1".into());
            writer.stage = DeliveryStage::Executing;
            writer.role = "executor".into();
            writer.status = StageRunStatus::Succeeded;
            writer.started_at_millis = 1_800_000_000_010;
            writer.finished_at_millis = Some(1_800_000_000_020);
            writer.delivery_task_id.clone()
        };
        let writer_binding = &mut snapshot.session_bindings[0];
        writer_binding.id = SessionBindingId("binding-executor-1".into());
        writer_binding.stage_run_id = StageRunId("stage-executor-1".into());
        writer_binding.product_session_id = ProductSessionId("product-executor".into());
        writer_binding.execution_job_id = ExecutionJobId("job-executor".into());
        writer_binding.worker_session_id = Some(WorkerSessionId("worker-executor".into()));
        writer_binding.codex_thread_id = Some(CodexThreadId("thread-executor".into()));
        writer_binding.bound_at_millis = 1_800_000_000_011;

        for (index, role) in ["reviewer", "verifier"].into_iter().enumerate() {
            let offset = u64::try_from(index).expect("small fixture index") * 20;
            let stage_run_id = StageRunId(format!("stage-{role}-1"));
            snapshot.stage_runs.push(StageRun {
                schema_version: super::super::DELIVERY_SCHEMA_VERSION,
                id: stage_run_id.clone(),
                delivery_id: snapshot.id.clone(),
                delivery_task_id: task_id.clone(),
                stage: DeliveryStage::Verifying,
                actor_type: StageRunActorType::Codex,
                role: role.into(),
                status: StageRunStatus::Succeeded,
                attempt: 1,
                started_at_millis: 1_800_000_000_030 + offset,
                finished_at_millis: Some(1_800_000_000_040 + offset),
            });
            snapshot.session_bindings.push(SessionBinding {
                schema_version: super::super::DELIVERY_SCHEMA_VERSION,
                id: SessionBindingId(format!("binding-{role}-1")),
                delivery_id: snapshot.id.clone(),
                delivery_task_id: task_id.clone(),
                stage_run_id,
                product_session_id: ProductSessionId(format!("product-{role}")),
                execution_job_id: ExecutionJobId(format!("job-{role}")),
                worker_session_id: Some(WorkerSessionId(format!("worker-{role}"))),
                codex_thread_id: Some(CodexThreadId(format!("thread-{role}"))),
                bound_at_millis: 1_800_000_000_031 + offset,
            });
        }
        snapshot.updated_at_millis = 1_800_000_000_060;
        Delivery::try_from_snapshot(snapshot).expect("verdict Delivery")
    }

    fn candidate(delivery: &Delivery) -> FrozenDeliveryCandidate {
        frozen_candidate(
            delivery,
            &StageRunId("stage-executor-1".into()),
            &SessionBindingId("binding-executor-1".into()),
        )
    }

    fn with_role_stage_status(
        delivery: &Delivery,
        role: VerificationRole,
        status: StageRunStatus,
    ) -> Delivery {
        let mut snapshot = delivery.clone().into_snapshot();
        let role = match role {
            VerificationRole::Reviewer => "reviewer",
            VerificationRole::Verifier => "verifier",
            VerificationRole::AdversarialVerifier => "adversarial-verifier",
        };
        let current_attempt = snapshot
            .stage_runs
            .iter()
            .filter(|run| run.stage == DeliveryStage::Verifying && run.role == role)
            .map(|run| run.attempt)
            .max()
            .expect("fixture role StageRun");
        let run = snapshot
            .stage_runs
            .iter_mut()
            .find(|run| {
                run.stage == DeliveryStage::Verifying
                    && run.role == role
                    && run.attempt == current_attempt
            })
            .expect("one current fixture role StageRun");
        run.status = status;
        if matches!(status, StageRunStatus::Running | StageRunStatus::Waiting) {
            run.finished_at_millis = None;
        }
        Delivery::try_from_snapshot(snapshot).expect("Delivery with current role status")
    }

    fn passing_inputs(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
    ) -> (
        crate::domain::verification::IndependentVerification,
        Vec<crate::domain::evidence::ResolvedDeliveryEvidence>,
    ) {
        let verification = independent_verification(
            delivery,
            candidate,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledPass,
        );
        let evidence = [VerificationRole::Reviewer, VerificationRole::Verifier]
            .into_iter()
            .map(|role| {
                role_evidence(
                    delivery,
                    candidate,
                    role,
                    EvidenceRefType::Test,
                    VerifiedEvidenceOutcome::Succeeded,
                )
            })
            .collect();
        (verification, evidence)
    }

    fn role_evidence(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
        role: VerificationRole,
        evidence_type: EvidenceRefType,
        outcome: VerifiedEvidenceOutcome,
    ) -> crate::domain::evidence::ResolvedDeliveryEvidence {
        let criterion_id = &delivery.snapshot().spec.acceptance_criteria[0].id;
        resolved_role_evidence(
            delivery,
            candidate,
            match role {
                VerificationRole::Reviewer => "reviewer",
                VerificationRole::Verifier => "verifier",
                VerificationRole::AdversarialVerifier => unreachable!(),
            },
            evidence_type,
            outcome,
            fixture_evidence_id(role, criterion_id),
        )
    }

    #[test]
    fn computes_deterministic_passing_verdict_from_sealed_facts() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let (verification, evidence) = passing_inputs(&delivery, &candidate);

        let first = compute_delivery_verdict(
            &delivery,
            &candidate,
            &verification,
            &evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect("passing verdict");
        let replay = compute_delivery_verdict(
            &delivery,
            &candidate,
            &verification,
            &evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect("deterministic replay");
        let later_materialization = compute_delivery_verdict(
            &delivery,
            &candidate,
            &verification,
            &evidence,
            PRODUCED_AT_MILLIS + 1,
        )
        .expect("same semantic verdict materialized later");

        assert_eq!(first, replay);
        assert_eq!(first.verdict().id, later_materialization.verdict().id);
        assert_eq!(
            first.verdict().criteria[0].id,
            later_materialization.verdict().criteria[0].id
        );
        assert_ne!(
            first.verdict().produced_at_millis,
            later_materialization.verdict().produced_at_millis
        );
        assert_eq!(first.verdict().status, CriterionVerdict::Pass);
        assert_eq!(first.verdict().criteria.len(), 2);
        assert_eq!(first.verdict().criteria[0].verdict, CriterionVerdict::Pass);
        assert_eq!(
            first.verdict().criteria[1].verdict,
            CriterionVerdict::Inconclusive
        );
        assert_eq!(first.evidence().len(), 2);
        assert_eq!(first.next_status(), DeliveryStatus::ReadyToDeliver);
    }

    #[test]
    fn source_sequence_must_match_the_finding_and_its_terminal() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let verification = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledPass,
        );
        let criterion_id = &delivery.snapshot().spec.acceptance_criteria[0].id;
        let evidence = vec![
            role_evidence(
                &delivery,
                &candidate,
                VerificationRole::Reviewer,
                EvidenceRefType::Test,
                VerifiedEvidenceOutcome::Succeeded,
            ),
            resolved_role_evidence_at_sequence(
                &delivery,
                &candidate,
                "verifier",
                EvidenceRefType::Test,
                VerifiedEvidenceOutcome::Succeeded,
                fixture_evidence_id(VerificationRole::Verifier, criterion_id),
                2,
            ),
        ];

        let computed = compute_delivery_verdict(
            &delivery,
            &candidate,
            &verification,
            &evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect("sequence mismatch is classified");

        assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
        assert!(
            computed
                .verdict()
                .unresolved_findings
                .iter()
                .any(|finding| finding.starts_with("evidence-mismatch:"))
        );
    }

    #[test]
    fn stale_verification_projection_cannot_authorize_a_later_role_attempt() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let (verification, evidence) = passing_inputs(&delivery, &candidate);
        let mut snapshot = delivery.into_snapshot();
        let task_id = snapshot.stage_runs[0].delivery_task_id.clone();
        let stage_run_id = StageRunId("stage-reviewer-2".into());
        snapshot.stage_runs.push(StageRun {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: stage_run_id.clone(),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: task_id.clone(),
            stage: DeliveryStage::Verifying,
            actor_type: StageRunActorType::Codex,
            role: "reviewer".into(),
            status: StageRunStatus::Succeeded,
            attempt: 2,
            started_at_millis: 1_800_000_000_070,
            finished_at_millis: Some(1_800_000_000_080),
        });
        snapshot.session_bindings.push(SessionBinding {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId("binding-reviewer-2".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: task_id,
            stage_run_id,
            product_session_id: ProductSessionId("product-reviewer-2".into()),
            execution_job_id: ExecutionJobId("job-reviewer-2".into()),
            worker_session_id: Some(WorkerSessionId("worker-reviewer-2".into())),
            codex_thread_id: Some(CodexThreadId("thread-reviewer-2".into())),
            bound_at_millis: 1_800_000_000_071,
        });
        snapshot.updated_at_millis = 1_800_000_000_080;
        let retried = Delivery::try_from_snapshot(snapshot).expect("retried verification Delivery");

        let error = compute_delivery_verdict(
            &retried,
            &candidate,
            &verification,
            &evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect_err("older settled projection must be stale after a later role attempt");

        assert_eq!(
            error.code(),
            crate::domain::DeliveryValidationErrorCode::RelationshipMismatch
        );
    }

    #[test]
    fn stale_verification_projection_rejects_mutated_terminal_stage_facts() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let (verification, evidence) = passing_inputs(&delivery, &candidate);

        for mutation in ["status", "attempt", "finished-at"] {
            let mut snapshot = delivery.clone().into_snapshot();
            let reviewer = snapshot
                .stage_runs
                .iter_mut()
                .find(|run| run.id.0 == "stage-reviewer-1")
                .expect("reviewer StageRun");
            match mutation {
                "status" => reviewer.status = StageRunStatus::Failed,
                "attempt" => reviewer.attempt += 1,
                "finished-at" => {
                    reviewer.finished_at_millis = reviewer
                        .finished_at_millis
                        .map(|finished_at_millis| finished_at_millis + 1);
                }
                _ => unreachable!(),
            }
            let changed = Delivery::try_from_snapshot(snapshot)
                .expect("aggregate permits stale projection detection at verdict time");

            compute_delivery_verdict(
                &changed,
                &candidate,
                &verification,
                &evidence,
                PRODUCED_AT_MILLIS,
            )
            .expect_err("mutated canonical terminal StageRun must stale the projection");
        }
    }

    #[test]
    fn agent_message_or_generic_runtime_event_cannot_produce_pass() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let verification = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledPass,
        );

        for evidence_type in [
            EvidenceRefType::RuntimeEvent,
            EvidenceRefType::ReviewFinding,
        ] {
            let evidence = [VerificationRole::Reviewer, VerificationRole::Verifier]
                .into_iter()
                .map(|role| {
                    role_evidence(
                        &delivery,
                        &candidate,
                        role,
                        evidence_type,
                        VerifiedEvidenceOutcome::Succeeded,
                    )
                })
                .collect::<Vec<_>>();
            let computed = compute_delivery_verdict(
                &delivery,
                &candidate,
                &verification,
                &evidence,
                PRODUCED_AT_MILLIS,
            )
            .expect("generic source remains visible but cannot pass");
            assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
            assert_eq!(computed.evidence().len(), 2);
            assert!(
                computed
                    .verdict()
                    .unresolved_findings
                    .iter()
                    .any(|finding| finding.starts_with("evidence-mismatch:"))
            );
        }
    }

    #[test]
    fn direct_evidence_outcome_cannot_be_reclassified_by_agent_claim() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        for (state, outcome) in [
            (
                VerificationFixtureState::SettledPass,
                VerifiedEvidenceOutcome::Failed,
            ),
            (
                VerificationFixtureState::SettledFail,
                VerifiedEvidenceOutcome::Succeeded,
            ),
        ] {
            let verification = independent_verification(&delivery, &candidate, state, state);
            let evidence = [VerificationRole::Reviewer, VerificationRole::Verifier]
                .into_iter()
                .map(|role| {
                    role_evidence(&delivery, &candidate, role, EvidenceRefType::Test, outcome)
                })
                .collect::<Vec<_>>();
            let computed = compute_delivery_verdict(
                &delivery,
                &candidate,
                &verification,
                &evidence,
                PRODUCED_AT_MILLIS,
            )
            .expect("direct outcome controls classification");
            assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
            assert_eq!(computed.evidence().len(), 2);
            assert!(
                computed
                    .verdict()
                    .unresolved_findings
                    .iter()
                    .all(|finding| finding.starts_with("evidence-mismatch:"))
            );
        }
    }

    #[test]
    fn failed_test_or_command_cannot_produce_pass() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let verification = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledPass,
        );
        for evidence_type in [EvidenceRefType::Test, EvidenceRefType::Command] {
            let evidence = [VerificationRole::Reviewer, VerificationRole::Verifier]
                .into_iter()
                .map(|role| {
                    role_evidence(
                        &delivery,
                        &candidate,
                        role,
                        evidence_type,
                        VerifiedEvidenceOutcome::Failed,
                    )
                })
                .collect::<Vec<_>>();
            let computed = compute_delivery_verdict(
                &delivery,
                &candidate,
                &verification,
                &evidence,
                PRODUCED_AT_MILLIS,
            )
            .expect("failed direct check stays an evidence mismatch");
            assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
            assert_ne!(computed.verdict().status, CriterionVerdict::Pass);
        }
    }

    #[test]
    fn missing_verification_method_is_inconclusive() {
        let mut snapshot = verdict_delivery().into_snapshot();
        snapshot.spec.acceptance_criteria[0].verification_method = None;
        let delivery = Delivery::try_from_snapshot(snapshot).expect("method-less Delivery");
        let candidate = candidate(&delivery);
        let (verification, evidence) = passing_inputs(&delivery, &candidate);

        let computed = compute_delivery_verdict(
            &delivery,
            &candidate,
            &verification,
            &evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect("missing method is classified");

        assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
        assert_eq!(
            computed.verdict().criteria[0].verdict,
            CriterionVerdict::Inconclusive
        );
        assert!(
            computed
                .verdict()
                .unresolved_findings
                .contains(&"verification-blocked:criterion-required:missing-method".into())
        );
        assert_eq!(computed.next_status(), DeliveryStatus::Clarifying);
    }

    #[test]
    fn missing_required_session_is_inconclusive() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        for state in [
            VerificationFixtureState::Missing,
            VerificationFixtureState::Incomplete,
        ] {
            let verification = independent_verification(
                &delivery,
                &candidate,
                state,
                VerificationFixtureState::SettledPass,
            );
            let evidence = vec![role_evidence(
                &delivery,
                &candidate,
                VerificationRole::Verifier,
                EvidenceRefType::Test,
                VerifiedEvidenceOutcome::Succeeded,
            )];
            let computed = compute_delivery_verdict(
                &delivery,
                &candidate,
                &verification,
                &evidence,
                PRODUCED_AT_MILLIS,
            )
            .expect("unfinished required role is classified");
            assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
            assert_ne!(computed.verdict().status, CriterionVerdict::Pass);
        }

        let running_delivery = with_role_stage_status(
            &delivery,
            VerificationRole::Reviewer,
            StageRunStatus::Running,
        );
        let running = validate_independent_verification(
            &running_delivery,
            &candidate,
            &VerificationFacts {
                required_roles: vec![VerificationRole::Reviewer, VerificationRole::Verifier],
                sessions: vec![VerificationSessionFacts {
                    role: VerificationRole::Reviewer,
                    stage_run_id: StageRunId("stage-reviewer-1".into()),
                    session_binding_id: SessionBindingId("binding-reviewer-1".into()),
                    workspace_mode: VerificationWorkspaceMode::CandidateReadOnly,
                    permission_profile: VerificationPermissionProfile::CandidateReadOnlyRestricted,
                    pre_candidate_snapshot: None,
                    post_candidate_snapshot: None,
                    accepted_job_outcome: None,
                    codex_turn_completed: false,
                    mutation_records: vec![],
                    findings: vec![],
                }],
            },
        )
        .expect("running role projection");
        assert_eq!(
            running.settlements()[0].state(),
            VerificationSessionState::Running
        );
        let computed = compute_delivery_verdict(
            &running_delivery,
            &candidate,
            &running,
            &[],
            PRODUCED_AT_MILLIS,
        )
        .expect("running required role is classified");
        assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
    }

    #[test]
    fn insufficient_direct_evidence_is_inconclusive() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let verification = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledPass,
        );
        let computed = compute_delivery_verdict(
            &delivery,
            &candidate,
            &verification,
            &[],
            PRODUCED_AT_MILLIS,
        )
        .expect("missing source Evidence is classified");

        assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
        assert!(
            computed
                .verdict()
                .unresolved_findings
                .iter()
                .all(|finding| finding.starts_with("evidence-mismatch:"))
        );
    }

    #[test]
    fn reviewer_verifier_conflict_is_inconclusive() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let verification = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledFail,
        );
        let evidence = vec![
            role_evidence(
                &delivery,
                &candidate,
                VerificationRole::Reviewer,
                EvidenceRefType::Command,
                VerifiedEvidenceOutcome::Succeeded,
            ),
            role_evidence(
                &delivery,
                &candidate,
                VerificationRole::Verifier,
                EvidenceRefType::Test,
                VerifiedEvidenceOutcome::Failed,
            ),
        ];
        let computed = compute_delivery_verdict(
            &delivery,
            &candidate,
            &verification,
            &evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect("role conflict is preserved");

        assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);
        assert_eq!(computed.evidence().len(), 2);
        assert!(
            computed
                .verdict()
                .unresolved_findings
                .iter()
                .any(|finding| {
                    finding == "contradiction:criterion-required:reviewer=pass,verifier=fail"
                })
        );
    }

    #[test]
    fn product_outcome_requires_all_required_roles_to_agree() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);

        let one_fail_one_missing = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledFail,
            VerificationFixtureState::Missing,
        );
        let reviewer_failure = vec![role_evidence(
            &delivery,
            &candidate,
            VerificationRole::Reviewer,
            EvidenceRefType::Test,
            VerifiedEvidenceOutcome::Failed,
        )];
        let computed = compute_delivery_verdict(
            &delivery,
            &candidate,
            &one_fail_one_missing,
            &reviewer_failure,
            PRODUCED_AT_MILLIS,
        )
        .expect("one role cannot decide product failure");
        assert_eq!(computed.verdict().status, CriterionVerdict::Inconclusive);

        let both_fail = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledFail,
            VerificationFixtureState::SettledFail,
        );
        let both_fail_evidence = [VerificationRole::Reviewer, VerificationRole::Verifier]
            .into_iter()
            .map(|role| {
                role_evidence(
                    &delivery,
                    &candidate,
                    role,
                    EvidenceRefType::Test,
                    VerifiedEvidenceOutcome::Failed,
                )
            })
            .collect::<Vec<_>>();
        let computed = compute_delivery_verdict(
            &delivery,
            &candidate,
            &both_fail,
            &both_fail_evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect("all required roles confirm product failure");
        assert_eq!(computed.verdict().status, CriterionVerdict::Fail);
        assert_eq!(computed.next_status(), DeliveryStatus::Reworking);

        let fail_and_infra = independent_verification(
            &with_role_stage_status(
                &delivery,
                VerificationRole::Verifier,
                StageRunStatus::Failed,
            ),
            &candidate,
            VerificationFixtureState::SettledFail,
            VerificationFixtureState::InfrastructureFailed,
        );
        let fail_and_infra_delivery = with_role_stage_status(
            &delivery,
            VerificationRole::Verifier,
            StageRunStatus::Failed,
        );
        let computed = compute_delivery_verdict(
            &fail_and_infra_delivery,
            &candidate,
            &fail_and_infra,
            &reviewer_failure,
            PRODUCED_AT_MILLIS,
        )
        .expect("environment failure is not overwritten by a product failure claim");
        assert_eq!(computed.verdict().status, CriterionVerdict::InfraError);
    }

    #[test]
    fn runtime_environment_failure_is_infra_error() {
        let delivery = verdict_delivery();
        let candidate = candidate(&delivery);
        let settled = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledPass,
        );
        for outcome in [
            VerifiedEvidenceOutcome::TimedOut,
            VerifiedEvidenceOutcome::PolicyDenied,
            VerifiedEvidenceOutcome::InfrastructureFailed,
            VerifiedEvidenceOutcome::Cancelled,
        ] {
            let evidence = [VerificationRole::Reviewer, VerificationRole::Verifier]
                .into_iter()
                .map(|role| {
                    role_evidence(&delivery, &candidate, role, EvidenceRefType::Test, outcome)
                })
                .collect::<Vec<_>>();
            let computed = compute_delivery_verdict(
                &delivery,
                &candidate,
                &settled,
                &evidence,
                PRODUCED_AT_MILLIS,
            )
            .expect("sealed environment outcome");
            assert_eq!(computed.verdict().status, CriterionVerdict::InfraError);
        }

        for state in [
            VerificationFixtureState::Failed,
            VerificationFixtureState::Cancelled,
        ] {
            let stage_status = match state {
                VerificationFixtureState::Failed => StageRunStatus::Failed,
                VerificationFixtureState::Cancelled => StageRunStatus::Cancelled,
                _ => unreachable!("loop only covers terminal environment failures"),
            };
            let terminal_delivery = with_role_stage_status(
                &with_role_stage_status(&delivery, VerificationRole::Reviewer, stage_status),
                VerificationRole::Verifier,
                stage_status,
            );
            let verification =
                independent_verification(&terminal_delivery, &candidate, state, state);
            let computed = compute_delivery_verdict(
                &terminal_delivery,
                &candidate,
                &verification,
                &[],
                PRODUCED_AT_MILLIS,
            )
            .expect("terminal verification environment failure");
            assert_eq!(computed.verdict().status, CriterionVerdict::InfraError);
        }
    }

    #[test]
    fn rejects_caller_supplied_evidence_results_verdict_or_attention() {
        let delivery = verdict_delivery();
        let mut value = serde_json::to_value(&delivery).expect("Delivery JSON");
        for field in [
            "callerEvidence",
            "callerCriterionResults",
            "callerVerdict",
            "callerAttention",
            "callerStatus",
        ] {
            value
                .as_object_mut()
                .expect("Delivery object")
                .insert(field.into(), serde_json::json!({ "requested": "pass" }));
            let encoded = serde_json::to_vec(&value).expect("caller request JSON");
            assert!(Delivery::decode_json(&encoded).is_err(), "accepted {field}");
            value
                .as_object_mut()
                .expect("Delivery object")
                .remove(field);
        }

        let candidate = candidate(&delivery);
        let (verification, evidence) = passing_inputs(&delivery, &candidate);
        let computed = compute_delivery_verdict(
            &delivery,
            &candidate,
            &verification,
            &evidence,
            PRODUCED_AT_MILLIS,
        )
        .expect("Control Plane derives canonical facts");
        assert!(
            computed
                .verdict()
                .id
                .0
                .starts_with("delivery-verdict:sha256:")
        );
        assert!(computed.verdict().criteria.iter().all(|result| {
            result.id.0.starts_with("criterion-result:sha256:")
                && result.evaluated_at_millis == PRODUCED_AT_MILLIS
        }));
    }

    #[test]
    fn ready_or_delivered_requires_passing_verdict() {
        let mut fixture = test_fixture();
        fixture.verdict.as_mut().expect("verdict").status = CriterionVerdict::Fail;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn ready_or_delivered_requires_completed_tasks() {
        let mut fixture = test_fixture();
        fixture.tasks[0].status = DeliveryTaskStatus::Active;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn pass_or_fail_criterion_requires_evidence() {
        let mut fixture = test_fixture();
        fixture.verdict.as_mut().expect("verdict").criteria[0]
            .evidence_refs
            .clear();
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn rejects_missing_duplicate_and_foreign_criterion_results() {
        let mut missing = test_fixture();
        missing.verdict.as_mut().expect("verdict").criteria.pop();
        assert!(Delivery::try_from_snapshot(missing).is_err());

        let mut duplicate = test_fixture();
        let mut repeated = duplicate.verdict.as_ref().expect("verdict").criteria[0].clone();
        repeated.id.0 = "criterion-result-duplicate".into();
        duplicate
            .verdict
            .as_mut()
            .expect("verdict")
            .criteria
            .push(repeated);
        assert!(Delivery::try_from_snapshot(duplicate).is_err());

        let mut foreign = test_fixture();
        foreign.verdict.as_mut().expect("verdict").criteria[0]
            .criterion_id
            .0 = "criterion-foreign".into();
        assert!(Delivery::try_from_snapshot(foreign).is_err());
    }

    #[test]
    fn delivery_verdict_status_folds_required_results_and_findings() {
        let mut fixture = test_fixture();
        fixture.status = DeliveryStatus::Verifying;
        let verdict = fixture.verdict.as_mut().expect("verdict");
        verdict
            .unresolved_findings
            .push("Review is incomplete".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }
}
