// SPDX-License-Identifier: Apache-2.0

//! Precise, bounded Delivery rework decisions.
//!
//! The values in this module are derived authorization facts. They are not an
//! extra persisted Delivery object and they do not schedule Codex work.

use std::collections::HashSet;

use winwincode_domain::{DeliveryTaskId, EvidenceId};

use super::{
    AcceptanceCriterionId, CandidatePathState, CriterionVerdict, Delivery, DeliveryValidationError,
    DeliveryValidationErrorCode, DeliveryVerdictStatus, FrozenDeliveryCandidate, StageRunActorType,
    StageRunStatus, assert_frozen_candidate_current, bounded_text, portable_identifier,
    validation_error,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReworkTargetFact {
    pub delivery_task_id: DeliveryTaskId,
    pub diagram_id: String,
    pub node_id: String,
    pub file_path: String,
    pub hunk_sha256: String,
    pub evidence_ref_ids: Vec<EvidenceId>,
}

/// The current diagram/diff projection resolved by the Control Plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentReworkScope {
    pub candidate_ref: String,
    pub diff_sha256: String,
    pub targets: Vec<ReworkTargetFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreciseReworkAnnotation {
    pub candidate_ref: String,
    pub diff_sha256: String,
    pub delivery_task_id: DeliveryTaskId,
    pub diagram_id: String,
    pub node_id: String,
    pub file_path: String,
    pub hunk_sha256: String,
    pub evidence_ref_ids: Vec<EvidenceId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReworkHistoryFact {
    /// Failed criteria from the prior candidate for this same Spec revision.
    pub prior_failed_criterion_ids: Vec<AcceptanceCriterionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReworkClarificationReason {
    AttemptLimitExhausted,
    RepeatedCriterionFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReworkAuthorization {
    candidate_ref: String,
    diff_sha256: String,
    delivery_task_id: DeliveryTaskId,
    next_attempt: u64,
    targets: Vec<ReworkTargetFact>,
}

impl ReworkAuthorization {
    pub fn writer_actor(&self) -> StageRunActorType {
        StageRunActorType::Codex
    }

    pub fn writer_role(&self) -> &'static str {
        "remediator"
    }

    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    pub fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

    pub fn delivery_task_id(&self) -> &DeliveryTaskId {
        &self.delivery_task_id
    }

    pub fn next_attempt(&self) -> u64 {
        self.next_attempt
    }

    pub fn targets(&self) -> &[ReworkTargetFact] {
        &self.targets
    }

    pub fn requires_full_reverification(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReworkDecision {
    Start(ReworkAuthorization),
    Clarify(ReworkClarificationReason),
}

/// Chooses clarification or returns one exact remediator authorization.
///
/// The caller supplies current diagram annotations, but cannot supply the
/// writer role or attempt number. All annotations must exactly equal a current
/// Control Plane projection target and cite current candidate-bound Evidence.
///
/// # Errors
///
/// Rejects stale candidates, non-failing verdicts, expanded scope, foreign
/// tasks/files/hunks, or missing/foreign Evidence references.
pub fn decide_precise_rework(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    scope: &CurrentReworkScope,
    annotations: &[PreciseReworkAnnotation],
    history: &ReworkHistoryFact,
) -> Result<ReworkDecision, DeliveryValidationError> {
    assert_frozen_candidate_current(delivery, candidate)?;
    let verdict = delivery.snapshot().verdict.as_ref().ok_or_else(|| {
        invalid_rework("rework requires the computed verdict for the current candidate")
    })?;
    if verdict.candidate_ref != candidate.candidate_ref()
        || verdict.status != DeliveryVerdictStatus::Fail
    {
        return Err(invalid_rework(
            "rework requires a failing verdict for the exact current candidate",
        ));
    }

    let rework_count = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| {
            run.stage == super::DeliveryStage::Reworking
                && run.actor_type == StageRunActorType::Codex
                && run.role == "remediator"
        })
        .count() as u64;
    if rework_count >= delivery.snapshot().spec.max_rework_attempts {
        return Ok(ReworkDecision::Clarify(
            ReworkClarificationReason::AttemptLimitExhausted,
        ));
    }
    let current_failures: HashSet<&str> = verdict
        .criteria
        .iter()
        .filter(|result| result.verdict == CriterionVerdict::Fail)
        .map(|result| result.criterion_id.0.as_str())
        .collect();
    if rework_count > 0
        && history
            .prior_failed_criterion_ids
            .iter()
            .any(|criterion_id| current_failures.contains(criterion_id.0.as_str()))
    {
        return Ok(ReworkDecision::Clarify(
            ReworkClarificationReason::RepeatedCriterionFailure,
        ));
    }

    let targets = validate_precise_scope(delivery, candidate, scope, annotations)?;
    let delivery_task_id = targets[0].delivery_task_id.clone();
    Ok(ReworkDecision::Start(ReworkAuthorization {
        candidate_ref: candidate.candidate_ref().to_owned(),
        diff_sha256: candidate.diff_sha256().to_owned(),
        delivery_task_id,
        next_attempt: rework_count + 1,
        targets,
    }))
}

fn validate_precise_scope(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    scope: &CurrentReworkScope,
    annotations: &[PreciseReworkAnnotation],
) -> Result<Vec<ReworkTargetFact>, DeliveryValidationError> {
    if annotations.is_empty()
        || scope.candidate_ref != candidate.candidate_ref()
        || scope.diff_sha256 != candidate.diff_sha256()
    {
        return Err(invalid_rework(
            "rework scope must identify the exact current candidate and diff",
        ));
    }
    let producer = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| &run.id == candidate.producer_stage_run_id())
        .ok_or_else(|| invalid_rework("candidate producer disappeared"))?;
    let producer_task_id = producer.delivery_task_id.as_ref().ok_or_else(|| {
        invalid_rework("precise task rework requires the original DeliveryTask scope")
    })?;
    let mut selected = Vec::with_capacity(annotations.len());
    let mut unique_targets = HashSet::with_capacity(annotations.len());
    for annotation in annotations {
        validate_annotation_shape(annotation)?;
        if annotation.candidate_ref != scope.candidate_ref
            || annotation.diff_sha256 != scope.diff_sha256
            || &annotation.delivery_task_id != producer_task_id
        {
            return Err(invalid_rework(
                "rework annotation expands the current candidate or DeliveryTask scope",
            ));
        }
        let key = (
            annotation.diagram_id.as_str(),
            annotation.node_id.as_str(),
            annotation.file_path.as_str(),
            annotation.hunk_sha256.as_str(),
        );
        if !unique_targets.insert(key) {
            return Err(invalid_rework(
                "rework annotations contain a duplicate target",
            ));
        }
        let target = scope
            .targets
            .iter()
            .find(|target| {
                target.delivery_task_id == annotation.delivery_task_id
                    && target.diagram_id == annotation.diagram_id
                    && target.node_id == annotation.node_id
                    && target.file_path == annotation.file_path
                    && target.hunk_sha256 == annotation.hunk_sha256
                    && same_evidence_set(&target.evidence_ref_ids, &annotation.evidence_ref_ids)
            })
            .ok_or_else(|| {
                invalid_rework("rework annotation does not match one visible current diagram hunk")
            })?;
        let path_is_current = candidate
            .changed_paths()
            .iter()
            .any(|path| path.path == annotation.file_path);
        if !path_is_current {
            return Err(invalid_rework(
                "rework file is outside the current candidate changed paths",
            ));
        }
        for evidence_id in &annotation.evidence_ref_ids {
            let current = delivery.snapshot().evidence.iter().any(|evidence| {
                evidence.id == *evidence_id
                    && evidence.delivery_id == *delivery.id()
                    && evidence.delivery_spec_id == delivery.snapshot().spec.id
                    && evidence.delivery_spec_revision == delivery.snapshot().spec.revision
                    && evidence.candidate_ref == candidate.candidate_ref()
            });
            if !current {
                return Err(invalid_rework(
                    "rework annotation cites missing or stale candidate Evidence",
                ));
            }
        }
        selected.push(target.clone());
    }
    Ok(selected)
}

fn validate_annotation_shape(
    annotation: &PreciseReworkAnnotation,
) -> Result<(), DeliveryValidationError> {
    portable_identifier(&annotation.delivery_task_id.0, "rework.deliveryTaskId")?;
    portable_identifier(&annotation.diagram_id, "rework.diagramId")?;
    portable_identifier(&annotation.node_id, "rework.nodeId")?;
    if !portable_path(&annotation.file_path)
        || !lowercase_sha256(&annotation.diff_sha256)
        || !lowercase_sha256(&annotation.hunk_sha256)
        || annotation.evidence_ref_ids.is_empty()
    {
        return Err(invalid_rework(
            "rework annotation needs one portable path, exact digests, and Evidence",
        ));
    }
    let mut evidence = HashSet::with_capacity(annotation.evidence_ref_ids.len());
    for id in &annotation.evidence_ref_ids {
        portable_identifier(&id.0, "rework.evidenceRefIds")?;
        if !evidence.insert(id.0.as_str()) {
            return Err(invalid_rework(
                "rework annotation contains duplicate Evidence references",
            ));
        }
    }
    bounded_text(&annotation.candidate_ref, "rework.candidateRef", 4_096)
}

fn same_evidence_set(left: &[EvidenceId], right: &[EvidenceId]) -> bool {
    left.len() == right.len()
        && left.iter().all(|id| right.contains(id))
        && right.iter().all(|id| left.contains(id))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReworkDeltaHunkFact {
    pub file_path: String,
    pub hunk_sha256: String,
}

/// Verifies the remediator's actual delta after execution, not just its prompt.
///
/// # Errors
///
/// Rejects output outside an authorized path/hunk or a replacement candidate
/// that did not come from a later remediator writer.
pub fn assert_remediator_output_in_scope(
    authorization: &ReworkAuthorization,
    previous_candidate: &FrozenDeliveryCandidate,
    replacement_candidate: &FrozenDeliveryCandidate,
    delta_hunks: &[ReworkDeltaHunkFact],
) -> Result<(), DeliveryValidationError> {
    if replacement_candidate.candidate_ref() == previous_candidate.candidate_ref()
        || replacement_candidate.producer_stage_run_id()
            == previous_candidate.producer_stage_run_id()
        || authorization.candidate_ref != previous_candidate.candidate_ref()
        || delta_hunks.is_empty()
    {
        return Err(invalid_rework(
            "rework must produce a replacement candidate from a new remediator writer",
        ));
    }
    for delta in delta_hunks {
        if !portable_path(&delta.file_path)
            || !lowercase_sha256(&delta.hunk_sha256)
            || !authorization.targets.iter().any(|target| {
                target.file_path == delta.file_path && target.hunk_sha256 == delta.hunk_sha256
            })
        {
            return Err(invalid_rework(
                "remediator output expanded beyond an authorized file and hunk",
            ));
        }
    }
    Ok(())
}

/// Checks that a replacement candidate has fresh independent verification and Verdict.
///
/// # Errors
///
/// Rejects reuse of prior candidate Evidence/Verdict or omission of either
/// required verification role.
pub fn assert_rework_fully_reverified(
    delivery: &Delivery,
    replacement: &FrozenDeliveryCandidate,
) -> Result<(), DeliveryValidationError> {
    assert_frozen_candidate_current(delivery, replacement)?;
    let producer = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| &run.id == replacement.producer_stage_run_id())
        .ok_or_else(|| invalid_rework("replacement producer is missing"))?;
    if producer.stage != super::DeliveryStage::Reworking
        || producer.role != "remediator"
        || producer.status != StageRunStatus::Succeeded
    {
        return Err(invalid_rework(
            "replacement candidate must come from a successful remediator",
        ));
    }
    for role in ["reviewer", "verifier"] {
        let verified = delivery.snapshot().stage_runs.iter().any(|run| {
            run.stage == super::DeliveryStage::Verifying
                && run.role == role
                && run.delivery_task_id == producer.delivery_task_id
                && run.status == StageRunStatus::Succeeded
                && run.started_at_millis
                    >= producer
                        .finished_at_millis
                        .unwrap_or(producer.started_at_millis)
                && delivery.snapshot().session_bindings.iter().any(|binding| {
                    binding.stage_run_id == run.id
                        && binding.worker_session_id.is_some()
                        && binding.codex_thread_id.is_some()
                })
        });
        if !verified {
            return Err(invalid_rework(
                "replacement candidate requires fresh Reviewer and Verifier sessions",
            ));
        }
    }
    let verdict = delivery
        .snapshot()
        .verdict
        .as_ref()
        .filter(|verdict| verdict.candidate_ref == replacement.candidate_ref())
        .ok_or_else(|| invalid_rework("replacement candidate needs a fresh DeliveryVerdict"))?;
    for result in &verdict.criteria {
        if result.candidate_ref != replacement.candidate_ref()
            || result.evidence_refs.iter().any(|evidence_id| {
                !delivery.snapshot().evidence.iter().any(|evidence| {
                    evidence.id == *evidence_id
                        && evidence.candidate_ref == replacement.candidate_ref()
                })
            })
        {
            return Err(invalid_rework(
                "replacement Verdict reuses missing or prior-candidate Evidence",
            ));
        }
    }
    Ok(())
}

fn portable_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4_096
        && !value.starts_with('/')
        && !value.contains('\\')
        && !value.bytes().any(|byte| byte <= 31 || byte == 127)
        && !value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn invalid_rework(message: &str) -> DeliveryValidationError {
    validation_error(
        DeliveryValidationErrorCode::RelationshipMismatch,
        "rework",
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CandidateGitSnapshotResolver, CriterionResult, DeliveryStatus, DeliveryVerdict,
        DeliveryVerdictId, EvidenceRef, EvidenceRefType, FreezeCandidateFacts, RepositoryRef,
        ResolvedGitCommit, ResolvedGitDiff, SessionBindingId, test_fixture,
    };
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, ProductSessionId, StageRunId, WorkerSessionId,
    };

    struct GitFixture {
        facts: FreezeCandidateFacts,
    }

    impl CandidateGitSnapshotResolver for GitFixture {
        fn resolve_commit(
            &self,
            _repository: &RepositoryRef,
            commit_id: &str,
        ) -> Result<ResolvedGitCommit, String> {
            let tree_id = if commit_id == self.facts.base_commit_id {
                self.facts.base_tree_id.clone()
            } else if commit_id == self.facts.candidate_commit_id {
                self.facts.candidate_tree_id.clone()
            } else {
                return Err("unknown commit".into());
            };
            Ok(ResolvedGitCommit {
                commit_id: commit_id.into(),
                tree_id,
            })
        }

        fn resolve_diff(
            &self,
            _repository: &RepositoryRef,
            base_commit_id: &str,
            candidate_commit_id: &str,
        ) -> Result<ResolvedGitDiff, String> {
            Ok(ResolvedGitDiff {
                base_commit_id: base_commit_id.into(),
                candidate_commit_id: candidate_commit_id.into(),
                diff_sha256: self.facts.diff_sha256.clone(),
                changed_paths: self.facts.changed_paths.clone(),
            })
        }
    }

    fn current_failure() -> (
        Delivery,
        FrozenDeliveryCandidate,
        CurrentReworkScope,
        PreciseReworkAnnotation,
    ) {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        let run = &mut snapshot.stage_runs[0];
        run.id = StageRunId("writer".into());
        run.stage = super::super::DeliveryStage::Executing;
        run.role = "executor".into();
        run.status = StageRunStatus::Succeeded;
        let binding = &mut snapshot.session_bindings[0];
        binding.id = SessionBindingId("writer-binding".into());
        binding.stage_run_id = run.id.clone();
        binding.product_session_id = ProductSessionId("writer-product".into());
        binding.execution_job_id = ExecutionJobId("writer-job".into());
        binding.worker_session_id = Some(WorkerSessionId("writer-worker".into()));
        binding.codex_thread_id = Some(CodexThreadId("writer-thread".into()));
        let writer = Delivery::try_from_snapshot(snapshot).expect("writer");
        let facts = FreezeCandidateFacts {
            producer_stage_run_id: StageRunId("writer".into()),
            producer_session_binding_id: SessionBindingId("writer-binding".into()),
            base_commit_id: "0123456789012345678901234567890123456789".into(),
            base_tree_id: "1".repeat(40),
            candidate_commit_id: "2".repeat(40),
            candidate_tree_id: "3".repeat(40),
            diff_sha256: "a".repeat(64),
            changed_paths: vec![super::super::CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("4".repeat(40)),
            }],
        };
        let git = GitFixture {
            facts: facts.clone(),
        };
        let candidate =
            super::super::freeze_delivery_candidate(&writer, facts, &git).expect("candidate");
        let mut snapshot = writer.into_snapshot();
        snapshot.status = DeliveryStatus::NeedsAttention;
        snapshot.evidence = vec![EvidenceRef {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: EvidenceId("evidence-failure".into()),
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            delivery_spec_revision: snapshot.spec.revision,
            stage_run_id: StageRunId("writer".into()),
            session_binding_id: SessionBindingId("writer-binding".into()),
            candidate_ref: candidate.candidate_ref().into(),
            evidence_type: EvidenceRefType::Test,
            source_ref: "runtime_event:test-failure".into(),
            created_at_millis: 1_800_000_000_019,
        }];
        snapshot.verdict = Some(DeliveryVerdict {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: DeliveryVerdictId("verdict-failure".into()),
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            candidate_ref: candidate.candidate_ref().into(),
            status: CriterionVerdict::Fail,
            criteria: snapshot
                .spec
                .acceptance_criteria
                .iter()
                .enumerate()
                .map(|(index, criterion)| CriterionResult {
                    schema_version: super::super::DELIVERY_SCHEMA_VERSION,
                    id: super::super::CriterionResultId(format!("result-{index}")),
                    delivery_id: snapshot.id.clone(),
                    delivery_spec_id: snapshot.spec.id.clone(),
                    criterion_id: criterion.id.clone(),
                    candidate_ref: candidate.candidate_ref().into(),
                    verdict: if criterion.required {
                        CriterionVerdict::Fail
                    } else {
                        CriterionVerdict::Inconclusive
                    },
                    evidence_refs: if criterion.required {
                        vec![EvidenceId("evidence-failure".into())]
                    } else {
                        vec![]
                    },
                    explanation: "computed result".into(),
                    evaluated_at_millis: 1_800_000_000_021,
                })
                .collect(),
            unresolved_findings: vec![],
            produced_at_millis: 1_800_000_000_022,
        });
        snapshot.attention_items.push(crate::domain::AttentionItem {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: winwincode_domain::AttentionItemId("rework-attention".into()),
            delivery_id: snapshot.id.clone(),
            delivery_spec_id: snapshot.spec.id.clone(),
            stage_run_id: Some(StageRunId("writer".into())),
            item_type: crate::domain::AttentionItemType::VerificationBlocked,
            title: "Rework failure".into(),
            context: "Required criterion failed".into(),
            options: vec![],
            assigned_to: Some("owner".into()),
            blocking: true,
            status: crate::domain::AttentionItemStatus::Open,
            resolution: None,
            resolved_by: None,
            created_at_millis: 1_800_000_000_023,
            resolved_at_millis: None,
        });
        snapshot.revision += 1;
        snapshot.updated_at_millis = 1_800_000_000_023;
        let delivery = Delivery::try_from_snapshot(snapshot).expect("failed Delivery");
        let target = ReworkTargetFact {
            delivery_task_id: DeliveryTaskId("delivery-task-api".into()),
            diagram_id: "diagram-main".into(),
            node_id: "node-api".into(),
            file_path: "src/invitation.rs".into(),
            hunk_sha256: "b".repeat(64),
            evidence_ref_ids: vec![EvidenceId("evidence-failure".into())],
        };
        let scope = CurrentReworkScope {
            candidate_ref: candidate.candidate_ref().into(),
            diff_sha256: candidate.diff_sha256().into(),
            targets: vec![target.clone()],
        };
        let annotation = PreciseReworkAnnotation {
            candidate_ref: scope.candidate_ref.clone(),
            diff_sha256: scope.diff_sha256.clone(),
            delivery_task_id: target.delivery_task_id.clone(),
            diagram_id: target.diagram_id.clone(),
            node_id: target.node_id.clone(),
            file_path: target.file_path.clone(),
            hunk_sha256: target.hunk_sha256.clone(),
            evidence_ref_ids: target.evidence_ref_ids.clone(),
        };
        (delivery, candidate, scope, annotation)
    }

    #[test]
    fn rework_requires_precise_current_candidate_annotations() {
        let (delivery, candidate, scope, annotation) = current_failure();
        let decision = decide_precise_rework(
            &delivery,
            &candidate,
            &scope,
            &[annotation.clone()],
            &ReworkHistoryFact {
                prior_failed_criterion_ids: vec![],
            },
        )
        .expect("authorization");
        assert!(matches!(decision, ReworkDecision::Start(_)));

        let mut stale = annotation;
        stale.hunk_sha256 = "0".repeat(64);
        assert!(
            decide_precise_rework(
                &delivery,
                &candidate,
                &scope,
                &[stale],
                &ReworkHistoryFact {
                    prior_failed_criterion_ids: vec![]
                },
            )
            .is_err()
        );
    }

    #[test]
    fn rework_is_bounded_and_remediator_owned() {
        let (delivery, candidate, scope, annotation) = current_failure();
        let ReworkDecision::Start(authorization) = decide_precise_rework(
            &delivery,
            &candidate,
            &scope,
            &[annotation],
            &ReworkHistoryFact {
                prior_failed_criterion_ids: vec![],
            },
        )
        .expect("decision") else {
            panic!("start")
        };
        assert_eq!(authorization.writer_actor(), StageRunActorType::Codex);
        assert_eq!(authorization.writer_role(), "remediator");
        assert_eq!(authorization.next_attempt(), 1);
    }

    #[test]
    fn rework_invalidates_previous_candidate_and_requires_full_reverification() {
        let (delivery, candidate, scope, annotation) = current_failure();
        let ReworkDecision::Start(authorization) = decide_precise_rework(
            &delivery,
            &candidate,
            &scope,
            &[annotation],
            &ReworkHistoryFact {
                prior_failed_criterion_ids: vec![],
            },
        )
        .expect("decision") else {
            panic!("start")
        };
        assert!(authorization.requires_full_reverification());
        assert!(assert_rework_fully_reverified(&delivery, &candidate).is_err());
    }

    #[test]
    fn repeated_or_exhausted_failure_requires_clarification() {
        let (mut delivery, candidate, scope, annotation) = current_failure();
        let required = delivery.snapshot().spec.acceptance_criteria[0].id.clone();
        let mut snapshot = delivery.into_snapshot();
        snapshot.spec.max_rework_attempts = 0;
        delivery = Delivery::try_from_snapshot(snapshot).expect("zero limit");
        assert_eq!(
            decide_precise_rework(
                &delivery,
                &candidate,
                &scope,
                &[annotation],
                &ReworkHistoryFact {
                    prior_failed_criterion_ids: vec![required]
                },
            )
            .expect("decision"),
            ReworkDecision::Clarify(ReworkClarificationReason::AttemptLimitExhausted)
        );
    }
}
