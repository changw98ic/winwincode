// SPDX-License-Identifier: Apache-2.0

//! Precise, bounded Delivery rework decisions.
//!
//! The values in this module are derived authorization facts. They are not an
//! extra persisted Delivery object and they do not schedule Codex work.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{DeliveryId, DeliveryTaskId, EvidenceId, Sha256Digest, StageRunId};

use super::{
    AcceptanceCriterionId, CriterionVerdict, Delivery, DeliverySnapshot, DeliveryStatus,
    DeliveryValidationError, DeliveryValidationErrorCode, DeliveryVerdictStatus,
    FreezeCandidateFacts, FrozenDeliveryCandidate, StageRunActorType, StageRunStatus,
    ValidatedGitSnapshotFact, assert_frozen_candidate_current, bounded_text,
    candidate::{assert_validated_git_snapshot_fact, freeze_authorized_rework_candidate},
    portable_identifier, validation_error,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReworkTargetFact {
    delivery_task_id: DeliveryTaskId,
    diagram_id: String,
    node_id: String,
    file_path: String,
    hunk_sha256: String,
    evidence_ref_ids: Vec<EvidenceId>,
}

impl ReworkTargetFact {
    pub fn delivery_task_id(&self) -> &DeliveryTaskId {
        &self.delivery_task_id
    }

    pub fn diagram_id(&self) -> &str {
        &self.diagram_id
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub fn hunk_sha256(&self) -> &str {
        &self.hunk_sha256
    }

    pub fn evidence_ref_ids(&self) -> &[EvidenceId] {
        &self.evidence_ref_ids
    }
}

/// The current diagram/diff projection resolved by the Control Plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentReworkScope {
    candidate_ref: String,
    diff_sha256: String,
    targets: Vec<ReworkTargetFact>,
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

/// Append-only prior verdict history validated for one current `DeliverySpec`.
///
/// Fields and construction stay crate-private so transport callers cannot
/// erase a prior failure by submitting an empty history. The Control Plane
/// transaction derives this fact from journal snapshots before asking for a
/// rework decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedReworkHistoryFact {
    delivery_id: DeliveryId,
    delivery_spec_id: super::DeliverySpecId,
    delivery_spec_revision: u64,
    observed_rework_count: u64,
    prior_failed_criterion_ids: Vec<AcceptanceCriterionId>,
    history_digest: Sha256Digest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReworkHistoryIdentity<'history> {
    delivery_id: &'history DeliveryId,
    delivery_spec_id: &'history super::DeliverySpecId,
    delivery_spec_revision: u64,
    observed_rework_count: u64,
    prior_failed_criterion_ids: &'history [AcceptanceCriterionId],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReworkClarificationReason {
    AttemptLimitExhausted,
    RepeatedCriterionFailure,
}

/// Constructor-derived proof that the current bounded rework policy requires
/// human clarification instead of another remediator attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReworkClarification {
    delivery_id: DeliveryId,
    delivery_spec_id: super::DeliverySpecId,
    delivery_spec_revision: u64,
    delivery_revision: u64,
    delivery_updated_at_millis: u64,
    history_digest: Sha256Digest,
    reason: ReworkClarificationReason,
    clarification_digest: Sha256Digest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReworkClarificationIdentity<'clarification> {
    delivery_id: &'clarification DeliveryId,
    delivery_spec_id: &'clarification super::DeliverySpecId,
    delivery_spec_revision: u64,
    delivery_revision: u64,
    delivery_updated_at_millis: u64,
    history_digest: &'clarification Sha256Digest,
    reason: &'clarification str,
}

impl ReworkClarification {
    pub const fn reason(&self) -> ReworkClarificationReason {
        self.reason
    }

    pub(crate) fn validate_for_transition(
        &self,
        delivery: &Delivery,
    ) -> Result<(), DeliveryValidationError> {
        let current = self.clarification_digest == seal_rework_clarification(self)?
            && self.delivery_id == *delivery.id()
            && self.delivery_spec_id == delivery.snapshot().spec.id
            && self.delivery_spec_revision == delivery.snapshot().spec.revision
            && self.delivery_revision == delivery.revision()
            && self.delivery_updated_at_millis == delivery.snapshot().updated_at_millis
            && delivery.snapshot().status == DeliveryStatus::Reworking;
        if current {
            Ok(())
        } else {
            Err(invalid_rework(
                "rework clarification is stale or belongs to another Delivery history",
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReworkAuthorization {
    previous_candidate: FrozenDeliveryCandidate,
    candidate_ref: String,
    diff_sha256: String,
    delivery_task_id: DeliveryTaskId,
    next_attempt: u64,
    targets: Vec<ReworkTargetFact>,
    authorized_delivery_revision: u64,
    authorized_delivery_updated_at_millis: u64,
    history_digest: Sha256Digest,
    authorization_digest: Sha256Digest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReworkAuthorizationIdentity<'authorization> {
    previous_candidate: &'authorization FrozenDeliveryCandidate,
    candidate_ref: &'authorization str,
    diff_sha256: &'authorization str,
    delivery_task_id: &'authorization DeliveryTaskId,
    next_attempt: u64,
    targets: &'authorization [ReworkTargetFact],
    authorized_delivery_revision: u64,
    authorized_delivery_updated_at_millis: u64,
    history_digest: &'authorization Sha256Digest,
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

    pub fn previous_candidate(&self) -> &FrozenDeliveryCandidate {
        &self.previous_candidate
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

    pub fn authorization_digest(&self) -> &Sha256Digest {
        &self.authorization_digest
    }

    pub fn requires_full_reverification(&self) -> bool {
        true
    }

    /// Revalidates this sealed decision immediately before dispatch.
    ///
    /// # Errors
    ///
    /// Rejects state drift, an invalidated candidate, changed failure facts,
    /// or a changed Delivery-wide attempt number.
    pub fn validate_for_dispatch(
        &self,
        delivery: &Delivery,
    ) -> Result<(), DeliveryValidationError> {
        if self.authorization_digest != seal_rework_authorization(self)? {
            return Err(invalid_rework(
                "rework authorization seal no longer matches its exact candidate and scope",
            ));
        }
        assert_frozen_candidate_current(delivery, &self.previous_candidate)?;
        let verdict = delivery.snapshot().verdict.as_ref().ok_or_else(|| {
            invalid_rework("rework dispatch requires the current failing verdict")
        })?;
        let failed_evidence_ids = verdict
            .criteria
            .iter()
            .filter(|result| result.verdict == CriterionVerdict::Fail)
            .flat_map(|result| result.evidence_refs.iter())
            .map(|evidence_id| evidence_id.0.as_str())
            .collect::<HashSet<_>>();
        let current = verdict.status == DeliveryVerdictStatus::Fail
            && verdict.candidate_ref == self.candidate_ref
            && self.previous_candidate.diff_sha256() == self.diff_sha256
            && self.previous_candidate.producer_delivery_task_id() == Some(&self.delivery_task_id)
            && next_rework_attempt(delivery) == self.next_attempt
            && self.next_attempt <= delivery.snapshot().spec.max_rework_attempts
            && authorization_revision_is_current(self, delivery)
            && !self.targets.is_empty()
            && self.targets.iter().all(|target| {
                target.delivery_task_id == self.delivery_task_id
                    && self
                        .previous_candidate
                        .changed_paths()
                        .iter()
                        .any(|path| path.path == target.file_path)
                    && !target.evidence_ref_ids.is_empty()
                    && target.evidence_ref_ids.iter().all(|evidence_id| {
                        failed_evidence_ids.contains(evidence_id.0.as_str())
                            && delivery.snapshot().evidence.iter().any(|evidence| {
                                evidence.id == *evidence_id
                                    && evidence.candidate_ref == self.candidate_ref
                                    && evidence.delivery_spec_revision
                                        == delivery.snapshot().spec.revision
                            })
                    })
            });
        if current {
            Ok(())
        } else {
            Err(invalid_rework(
                "rework authorization is stale or no longer matches dispatch facts",
            ))
        }
    }

    /// Revalidates the consumed authorization against the one newly-started
    /// remediator run. This is used after candidate Evidence and Verdict have
    /// been invalidated, so it relies on the sealed source candidate plus the
    /// exact post-advance Delivery snapshot instead of caller payload fields.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryValidationError`] when the authorization seal or any
    /// post-start `Delivery`, `StageRun`, `SessionBinding`, candidate, or
    /// history fact no longer matches the authorized rework attempt.
    pub fn validate_started_dispatch(
        &self,
        delivery: &Delivery,
        stage_run_id: &StageRunId,
    ) -> Result<(), DeliveryValidationError> {
        if self.authorization_digest != seal_rework_authorization(self)?
            || delivery.revision() != self.authorized_delivery_revision.saturating_add(1)
            || delivery.snapshot().status != DeliveryStatus::Reworking
            || !delivery.snapshot().evidence.is_empty()
            || delivery.snapshot().verdict.is_some()
        {
            return Err(invalid_rework(
                "started rework no longer matches the consumed sealed authorization",
            ));
        }
        let run = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| &run.id == stage_run_id)
            .ok_or_else(|| invalid_rework("authorized remediator StageRun is missing"))?;
        let exact_run = run.stage == super::DeliveryStage::Reworking
            && run.actor_type == StageRunActorType::Codex
            && run.role == self.writer_role()
            && run.status == StageRunStatus::Running
            && run.delivery_task_id.as_ref() == Some(&self.delivery_task_id)
            && run.attempt == self.next_attempt
            && run.started_at_millis >= self.authorized_delivery_updated_at_millis
            && next_rework_attempt(delivery).saturating_sub(1) == self.next_attempt;
        let source_run = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|source| &source.id == self.previous_candidate.producer_stage_run_id());
        let source_binding = delivery.snapshot().session_bindings.iter().find(|binding| {
            &binding.id == self.previous_candidate.producer_session_binding_id()
                && binding.stage_run_id == *self.previous_candidate.producer_stage_run_id()
        });
        let exact_source = source_run.is_some_and(|source| {
            source.status == StageRunStatus::Succeeded
                && source.finished_at_millis
                    == Some(self.previous_candidate.producer_finished_at_millis())
                && source.delivery_task_id.as_ref() == Some(&self.delivery_task_id)
        }) && source_binding.is_some_and(|binding| {
            binding.product_session_id == *self.previous_candidate.producer_product_session_id()
                && binding.execution_job_id == *self.previous_candidate.producer_execution_job_id()
                && binding.worker_session_id.as_ref()
                    == Some(self.previous_candidate.producer_worker_session_id())
                && binding.codex_thread_id.as_ref()
                    == Some(self.previous_candidate.producer_codex_thread_id())
        });
        if exact_run && exact_source {
            Ok(())
        } else {
            Err(invalid_rework(
                "started rework changed its source terminal, task, attempt, or remediator identity",
            ))
        }
    }
}

fn authorization_revision_is_current(
    authorization: &ReworkAuthorization,
    delivery: &Delivery,
) -> bool {
    delivery.revision() == authorization.authorized_delivery_revision
        && delivery.snapshot().updated_at_millis
            == authorization.authorized_delivery_updated_at_millis
}

fn seal_rework_authorization(
    authorization: &ReworkAuthorization,
) -> Result<Sha256Digest, DeliveryValidationError> {
    let identity = ReworkAuthorizationIdentity {
        previous_candidate: &authorization.previous_candidate,
        candidate_ref: &authorization.candidate_ref,
        diff_sha256: &authorization.diff_sha256,
        delivery_task_id: &authorization.delivery_task_id,
        next_attempt: authorization.next_attempt,
        targets: &authorization.targets,
        authorized_delivery_revision: authorization.authorized_delivery_revision,
        authorized_delivery_updated_at_millis: authorization.authorized_delivery_updated_at_millis,
        history_digest: &authorization.history_digest,
    };
    let encoded = serde_json::to_vec(&identity).map_err(|error| {
        invalid_rework(&format!(
            "rework authorization seal cannot be encoded: {error}"
        ))
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn seal_rework_clarification(
    clarification: &ReworkClarification,
) -> Result<Sha256Digest, DeliveryValidationError> {
    let identity = ReworkClarificationIdentity {
        delivery_id: &clarification.delivery_id,
        delivery_spec_id: &clarification.delivery_spec_id,
        delivery_spec_revision: clarification.delivery_spec_revision,
        delivery_revision: clarification.delivery_revision,
        delivery_updated_at_millis: clarification.delivery_updated_at_millis,
        history_digest: &clarification.history_digest,
        reason: match clarification.reason {
            ReworkClarificationReason::AttemptLimitExhausted => "attempt_limit_exhausted",
            ReworkClarificationReason::RepeatedCriterionFailure => "repeated_criterion_failure",
        },
    };
    let encoded = serde_json::to_vec(&identity).map_err(|error| {
        invalid_rework(&format!(
            "rework clarification seal cannot be encoded: {error}"
        ))
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn derive_rework_clarification(
    delivery: &Delivery,
    history: &ValidatedReworkHistoryFact,
    reason: ReworkClarificationReason,
) -> Result<ReworkClarification, DeliveryValidationError> {
    let mut clarification = ReworkClarification {
        delivery_id: delivery.id().clone(),
        delivery_spec_id: delivery.snapshot().spec.id.clone(),
        delivery_spec_revision: delivery.snapshot().spec.revision,
        delivery_revision: delivery.revision(),
        delivery_updated_at_millis: delivery.snapshot().updated_at_millis,
        history_digest: history.history_digest.clone(),
        reason,
        clarification_digest: Sha256Digest(String::new()),
    };
    clarification.clarification_digest = seal_rework_clarification(&clarification)?;
    Ok(clarification)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReworkDecision {
    Start(Box<ReworkAuthorization>),
    Clarify(ReworkClarification),
}

/// Blocking follow-up actions derived from all current Verdict Attention items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerdictAttentionAction {
    ClarifyDefinition,
    StartRework,
    RetryVerification,
}

/// Reconstructs the derived Verdict action from one resolved current
/// `AttentionItem`. The action itself is deliberately not persisted: the
/// canonical item type and resolution are the authority.
pub(crate) const fn resolved_verdict_attention_action(
    item_type: super::AttentionItemType,
    status: super::AttentionItemStatus,
) -> Option<VerdictAttentionAction> {
    match (item_type, status) {
        (
            super::AttentionItemType::ScopeChange,
            super::AttentionItemStatus::Resolved | super::AttentionItemStatus::Dismissed,
        )
        | (super::AttentionItemType::RequirementQuestion, super::AttentionItemStatus::Dismissed) => {
            Some(VerdictAttentionAction::ClarifyDefinition)
        }
        (super::AttentionItemType::VerificationBlocked, super::AttentionItemStatus::Dismissed) => {
            Some(VerdictAttentionAction::StartRework)
        }
        (super::AttentionItemType::VerificationBlocked, super::AttentionItemStatus::Resolved) => {
            Some(VerdictAttentionAction::RetryVerification)
        }
        _ => None,
    }
}

/// Chooses the safest next state from the complete current action set.
///
/// Clarification always outranks code rework, which outranks a retry. The
/// result therefore cannot change with Attention resolution order.
pub(crate) fn safest_attention_transition(actions: &[VerdictAttentionAction]) -> DeliveryStatus {
    if actions.contains(&VerdictAttentionAction::ClarifyDefinition) {
        DeliveryStatus::Clarifying
    } else if actions.contains(&VerdictAttentionAction::StartRework) {
        DeliveryStatus::Reworking
    } else {
        DeliveryStatus::Verifying
    }
}

/// Returns one plus every reworking `StageRun` in the current Delivery.
pub(crate) fn next_rework_attempt(delivery: &Delivery) -> u64 {
    delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| run.stage == super::DeliveryStage::Reworking)
        .count() as u64
        + 1
}

/// Derives sealed rework history from append-only Delivery journal snapshots.
///
/// The transaction may provide the full Delivery history. Only verdicts for
/// the current Spec revision and an earlier candidate are included. Missing,
/// reordered, duplicate, foreign, or insufficient history is rejected.
pub(crate) fn derive_validated_rework_history(
    delivery: &Delivery,
    append_only_history: &[DeliverySnapshot],
) -> Result<ValidatedReworkHistoryFact, DeliveryValidationError> {
    let mut previous_revision = None;
    let mut prior_candidate_refs = HashSet::new();
    let mut prior_failed_criterion_ids = HashSet::new();
    let current_candidate_ref = delivery
        .snapshot()
        .verdict
        .as_ref()
        .map(|verdict| verdict.candidate_ref.as_str());

    for historical in append_only_history {
        if historical.id != *delivery.id()
            || historical.revision >= delivery.revision()
            || historical.updated_at_millis > delivery.snapshot().updated_at_millis
            || previous_revision.is_some_and(|revision| historical.revision <= revision)
        {
            return Err(invalid_rework(
                "rework history must be one ordered earlier journal for the current Delivery",
            ));
        }
        previous_revision = Some(historical.revision);
        Delivery::try_from_snapshot(historical.clone())
            .map_err(|_| invalid_rework("rework history contains an invalid Delivery snapshot"))?;
        if historical.spec.id != delivery.snapshot().spec.id
            || historical.spec.revision != delivery.snapshot().spec.revision
        {
            continue;
        }
        let Some(verdict) = historical.verdict.as_ref() else {
            continue;
        };
        if verdict.status != DeliveryVerdictStatus::Fail
            || current_candidate_ref == Some(verdict.candidate_ref.as_str())
            || !prior_candidate_refs.insert(verdict.candidate_ref.as_str())
        {
            continue;
        }
        prior_failed_criterion_ids.extend(
            verdict
                .criteria
                .iter()
                .filter(|result| result.verdict == CriterionVerdict::Fail)
                .map(|result| result.criterion_id.clone()),
        );
    }

    let observed_rework_count = next_rework_attempt(delivery) - 1;
    if u64::try_from(prior_candidate_refs.len()).unwrap_or(u64::MAX) < observed_rework_count {
        return Err(invalid_rework(
            "append-only history is missing a prior failing candidate for a rework attempt",
        ));
    }
    let mut prior_failed_criterion_ids = prior_failed_criterion_ids.into_iter().collect::<Vec<_>>();
    prior_failed_criterion_ids.sort_by(|left, right| left.0.cmp(&right.0));
    let mut fact = ValidatedReworkHistoryFact {
        delivery_id: delivery.id().clone(),
        delivery_spec_id: delivery.snapshot().spec.id.clone(),
        delivery_spec_revision: delivery.snapshot().spec.revision,
        observed_rework_count,
        prior_failed_criterion_ids,
        history_digest: Sha256Digest(String::new()),
    };
    fact.history_digest = seal_rework_history(&fact)?;
    Ok(fact)
}

fn seal_rework_history(
    history: &ValidatedReworkHistoryFact,
) -> Result<Sha256Digest, DeliveryValidationError> {
    let identity = ReworkHistoryIdentity {
        delivery_id: &history.delivery_id,
        delivery_spec_id: &history.delivery_spec_id,
        delivery_spec_revision: history.delivery_spec_revision,
        observed_rework_count: history.observed_rework_count,
        prior_failed_criterion_ids: &history.prior_failed_criterion_ids,
    };
    let encoded = serde_json::to_vec(&identity).map_err(|error| {
        invalid_rework(&format!("rework history seal cannot be encoded: {error}"))
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

/// Removes prior candidate authorization when a remediator writer starts.
///
/// Historical records stay in the append-only journal, while the new current
/// snapshot no longer exposes prior-candidate Evidence or Verdict as usable.
pub(crate) fn invalidate_candidate_authorization_for_writer_start(snapshot: &mut DeliverySnapshot) {
    snapshot.evidence.clear();
    snapshot.verdict = None;
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
    history: &ValidatedReworkHistoryFact,
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

    let rework_count = next_rework_attempt(delivery) - 1;
    if history.delivery_id != *delivery.id()
        || history.delivery_spec_id != delivery.snapshot().spec.id
        || history.delivery_spec_revision != delivery.snapshot().spec.revision
        || history.observed_rework_count != rework_count
        || history.history_digest != seal_rework_history(history)?
    {
        return Err(invalid_rework(
            "validated rework history is stale or belongs to another DeliverySpec",
        ));
    }
    let current_failures: HashSet<&str> = verdict
        .criteria
        .iter()
        .filter(|result| result.verdict == CriterionVerdict::Fail)
        .map(|result| result.criterion_id.0.as_str())
        .collect();
    let failed_evidence_ids: HashSet<&str> = verdict
        .criteria
        .iter()
        .filter(|result| result.verdict == CriterionVerdict::Fail)
        .flat_map(|result| result.evidence_refs.iter())
        .map(|evidence_id| evidence_id.0.as_str())
        .collect();
    if let Some(reason) = clarification_reason(
        rework_count,
        delivery.snapshot().spec.max_rework_attempts,
        &current_failures,
        history,
    ) {
        return derive_rework_clarification(delivery, history, reason).map(ReworkDecision::Clarify);
    }

    let targets = validate_precise_scope(
        delivery,
        candidate,
        scope,
        annotations,
        &failed_evidence_ids,
    )?;
    let delivery_task_id = targets[0].delivery_task_id.clone();
    let mut authorization = ReworkAuthorization {
        previous_candidate: candidate.clone(),
        candidate_ref: candidate.candidate_ref().to_owned(),
        diff_sha256: candidate.diff_sha256().to_owned(),
        delivery_task_id,
        next_attempt: rework_count + 1,
        targets,
        authorized_delivery_revision: delivery.revision(),
        authorized_delivery_updated_at_millis: delivery.snapshot().updated_at_millis,
        history_digest: history.history_digest.clone(),
        authorization_digest: Sha256Digest(String::new()),
    };
    authorization.authorization_digest = seal_rework_authorization(&authorization)?;
    Ok(ReworkDecision::Start(Box::new(authorization)))
}

#[cfg(test)]
pub(crate) fn fixture_precise_rework_authorization(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    history: &ValidatedReworkHistoryFact,
    hunk_sha256: String,
) -> ReworkAuthorization {
    let delivery_task_id = candidate
        .producer_delivery_task_id()
        .expect("test candidate task")
        .clone();
    let file_path = candidate
        .changed_paths()
        .first()
        .expect("test candidate path")
        .path
        .clone();
    let evidence_ref_ids = delivery
        .snapshot()
        .verdict
        .as_ref()
        .expect("test failing verdict")
        .criteria
        .iter()
        .filter(|result| result.verdict == CriterionVerdict::Fail)
        .flat_map(|result| result.evidence_refs.iter().cloned())
        .collect::<Vec<_>>();
    let target = ReworkTargetFact {
        delivery_task_id: delivery_task_id.clone(),
        diagram_id: "diagram-current-verdict".into(),
        node_id: "node-current-failure".into(),
        file_path: file_path.clone(),
        hunk_sha256: hunk_sha256.clone(),
        evidence_ref_ids: evidence_ref_ids.clone(),
    };
    let scope = CurrentReworkScope {
        candidate_ref: candidate.candidate_ref().into(),
        diff_sha256: candidate.diff_sha256().into(),
        targets: vec![target],
    };
    let annotation = PreciseReworkAnnotation {
        candidate_ref: candidate.candidate_ref().into(),
        diff_sha256: candidate.diff_sha256().into(),
        delivery_task_id,
        diagram_id: "diagram-current-verdict".into(),
        node_id: "node-current-failure".into(),
        file_path,
        hunk_sha256,
        evidence_ref_ids,
    };
    let ReworkDecision::Start(authorization) =
        decide_precise_rework(delivery, candidate, &scope, &[annotation], history)
            .expect("test rework authorization")
    else {
        panic!("test failure should authorize bounded rework");
    };
    *authorization
}

fn clarification_reason(
    rework_count: u64,
    maximum: u64,
    current_failures: &HashSet<&str>,
    history: &ValidatedReworkHistoryFact,
) -> Option<ReworkClarificationReason> {
    if rework_count >= maximum {
        Some(ReworkClarificationReason::AttemptLimitExhausted)
    } else if rework_count > 0
        && history
            .prior_failed_criterion_ids
            .iter()
            .any(|criterion_id| current_failures.contains(criterion_id.0.as_str()))
    {
        Some(ReworkClarificationReason::RepeatedCriterionFailure)
    } else {
        None
    }
}

fn validate_precise_scope(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    scope: &CurrentReworkScope,
    annotations: &[PreciseReworkAnnotation],
    failed_evidence_ids: &HashSet<&str>,
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
                    && failed_evidence_ids.contains(evidence.id.0.as_str())
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

/// Atomically freezes and authorizes one remediator replacement candidate.
///
/// This is the only production entry that can turn remediator output into a
/// frozen candidate. The generic candidate freezer accepts executors only, so
/// skipping the scope check cannot create a verification-eligible candidate.
///
/// # Errors
///
/// Rejects stale authorization, another writer or fenced Job, changed Spec
/// lineage, and any old-to-new path or hunk outside the approved scope.
pub fn freeze_rework_replacement_candidate(
    delivery: &Delivery,
    authorization: &ReworkAuthorization,
    replacement_facts: &FreezeCandidateFacts,
    replacement_delta: &ValidatedGitSnapshotFact,
) -> Result<FrozenDeliveryCandidate, DeliveryValidationError> {
    if !replacement_delta.has_same_terminal_workspace(replacement_facts.git_snapshot()) {
        return Err(invalid_rework(
            "replacement delta and candidate snapshot must come from one exact terminal fenced workspace",
        ));
    }
    let replacement_candidate = freeze_authorized_rework_candidate(delivery, replacement_facts)?;
    assert_remediator_output_in_scope(
        delivery,
        authorization,
        &authorization.previous_candidate,
        &replacement_candidate,
        replacement_delta,
    )?;
    Ok(replacement_candidate)
}

/// Verifies a sealed old-to-new remediator snapshot after execution.
///
/// This check consumes the Git/Artifact adapter's opaque snapshot. Prompt text
/// and caller-provided path lists are not result authority.
fn assert_remediator_output_in_scope(
    delivery: &Delivery,
    authorization: &ReworkAuthorization,
    previous_candidate: &FrozenDeliveryCandidate,
    replacement_candidate: &FrozenDeliveryCandidate,
    replacement_delta: &ValidatedGitSnapshotFact,
) -> Result<(), DeliveryValidationError> {
    assert_frozen_candidate_current(delivery, replacement_candidate)?;
    assert_validated_git_snapshot_fact(replacement_delta)?;
    let producer = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| &run.id == replacement_candidate.producer_stage_run_id())
        .ok_or_else(|| invalid_rework("replacement remediator StageRun is missing"))?;
    let exact_writer = producer.stage == super::DeliveryStage::Reworking
        && producer.actor_type == StageRunActorType::Codex
        && producer.role == "remediator"
        && producer.status == StageRunStatus::Succeeded
        && producer.delivery_task_id.as_ref() == Some(&authorization.delivery_task_id)
        && producer.attempt == authorization.next_attempt
        && next_rework_attempt(delivery).saturating_sub(1) == authorization.next_attempt;
    let exact_lineage = previous_candidate.delivery_id() == replacement_candidate.delivery_id()
        && previous_candidate.delivery_spec_id() == replacement_candidate.delivery_spec_id()
        && previous_candidate.delivery_spec_revision()
            == replacement_candidate.delivery_spec_revision()
        && previous_candidate.repository() == replacement_candidate.repository()
        && previous_candidate.base_revision() == replacement_candidate.base_revision();
    let exact_delta = replacement_delta.repository() == replacement_candidate.repository()
        && replacement_delta.stage_run_id() == replacement_candidate.producer_stage_run_id()
        && replacement_delta.session_binding_id()
            == replacement_candidate.producer_session_binding_id()
        && replacement_delta.product_session_id()
            == replacement_candidate.producer_product_session_id()
        && replacement_delta.execution_job_id()
            == replacement_candidate.producer_execution_job_id()
        && replacement_delta.worker_session_id()
            == replacement_candidate.producer_worker_session_id()
        && replacement_delta.codex_thread_id() == replacement_candidate.producer_codex_thread_id()
        && replacement_delta.attempt() == replacement_candidate.producer_attempt()
        && replacement_delta.base_commit_id() == previous_candidate.candidate_commit_id()
        && replacement_delta.base_tree_id() == previous_candidate.candidate_tree_id()
        && replacement_delta.candidate_commit_id() == replacement_candidate.candidate_commit_id()
        && replacement_delta.candidate_tree_id() == replacement_candidate.candidate_tree_id();
    if replacement_candidate.candidate_ref() == previous_candidate.candidate_ref()
        || replacement_candidate.producer_stage_run_id()
            == previous_candidate.producer_stage_run_id()
        || authorization.candidate_ref != previous_candidate.candidate_ref()
        || !exact_writer
        || !exact_lineage
        || !exact_delta
        || replacement_delta.changed_paths().is_empty()
        || replacement_delta.changed_hunks().is_empty()
    {
        return Err(invalid_rework(
            "rework needs one sealed old-to-new snapshot from the authorized remediator",
        ));
    }
    for path in replacement_delta.changed_paths() {
        if !authorization
            .targets
            .iter()
            .any(|target| target.file_path == path.path)
            || !replacement_delta
                .changed_hunks()
                .iter()
                .any(|hunk| hunk.file_path == path.path)
        {
            return Err(invalid_rework(
                "remediator output added a path outside the authorization",
            ));
        }
    }
    for hunk in replacement_delta.changed_hunks() {
        if !authorization.targets.iter().any(|target| {
            target.file_path == hunk.file_path
                && hunk.source_hunk_sha256.as_deref() == Some(target.hunk_sha256.as_str())
        }) {
            return Err(invalid_rework(
                "remediator output expanded beyond an authorized file and source hunk range",
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

/// Narrow sealed fixtures for cross-crate rework transaction tests. They are
/// compiled only for tests and still pass through the production verdict,
/// Attention, candidate, history, decision, and stage application services.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{
        CriterionVerdict, CurrentReworkScope, Delivery, DeliveryStatus, EvidenceId,
        FrozenDeliveryCandidate, PreciseReworkAnnotation, ReworkAuthorization,
        ReworkClarificationReason, ReworkDecision, ReworkTargetFact, decide_precise_rework,
        derive_rework_clarification, derive_validated_rework_history,
        freeze_rework_replacement_candidate,
    };
    use crate::application::attention::ResolvedAttentionTransition;
    use crate::application::{
        attention::{AttentionDecision, ResolveAttentionInput, resolve_attention},
        stage::{AdvanceStageInput, NewStageIdentities, StageAdvanceResult, advance_rework},
        verdict::{
            ComputedVerdictTransition, SubmitVerdictFacts, compute_verdict_transition,
            test_support::{VerdictFixtureOutcome, verdict_fixture_with_rework_limit},
        },
    };
    use crate::domain::{
        SessionBindingId,
        candidate::test_support::{CandidateFixtureInput, rework_facts_and_delta_from_input},
    };
    use winwincode_domain::{
        AttentionItemId, DeliveryId, ExecutionJobId, ProductSessionId, StageRunId,
    };

    #[derive(Debug)]
    pub struct ReworkJournalFixture {
        pub initial_delivery: Delivery,
        pub verdict_transition: ComputedVerdictTransition,
        pub attention_transition: ResolvedAttentionTransition,
    }

    #[derive(Debug)]
    pub struct ReworkDispatchFixture {
        pub journal: ReworkJournalFixture,
        pub source_delivery: Delivery,
        pub transition: StageAdvanceResult,
        pub candidate_ref: String,
        pub source_candidate_commit_id: String,
        pub evidence_ref_ids: Vec<EvidenceId>,
    }

    #[derive(Debug)]
    pub struct ReworkClarificationFixture {
        pub journal: ReworkJournalFixture,
        pub source_delivery: Delivery,
        pub transition: StageAdvanceResult,
        pub reason: ReworkClarificationReason,
    }

    /// Freezes one authorization-bound remediator replacement through the
    /// production candidate and rework seams.
    ///
    /// # Panics
    ///
    /// Panics when the producer is not the current authorized remediator, its
    /// binding is foreign, or the observable old-to-new Git/artifact values
    /// exceed the exact authorized scope.
    #[must_use]
    pub fn freeze_rework_replacement_candidate_fixture(
        delivery: &Delivery,
        authorization: &ReworkAuthorization,
        producer_stage_run_id: &StageRunId,
        producer_session_binding_id: &SessionBindingId,
        input: CandidateFixtureInput,
    ) -> FrozenDeliveryCandidate {
        let (replacement_facts, delta) = rework_facts_and_delta_from_input(
            delivery,
            producer_stage_run_id,
            producer_session_binding_id,
            input,
        );
        freeze_rework_replacement_candidate(delivery, authorization, &replacement_facts, &delta)
            .expect("rework fixture must match the current authorization and sealed observations")
    }

    /// Builds one production-derived, sealed rework dispatch fixture.
    ///
    /// # Panics
    ///
    /// Panics if the fixed test facts no longer form a valid rework transition.
    #[must_use]
    pub fn authorized_rework_dispatch(delivery_id: &DeliveryId) -> ReworkDispatchFixture {
        let (journal, source_delivery, candidate, decision, evidence_ref_ids) =
            rework_decision_fixture(delivery_id, 3);
        let candidate_ref = candidate.candidate_ref().to_owned();
        let source_candidate_commit_id = candidate.candidate_commit_id().to_owned();
        let transition =
            advance_rework(&source_delivery, advance_input(&source_delivery), decision)
                .expect("authorized rework fixture must advance");
        ReworkDispatchFixture {
            journal,
            source_delivery,
            transition,
            candidate_ref,
            source_candidate_commit_id,
            evidence_ref_ids,
        }
    }

    /// Builds one production-sealed repeated-failure clarification fixture.
    ///
    /// # Panics
    ///
    /// Panics if the fixed test facts no longer form a valid clarification transition.
    #[must_use]
    pub fn repeated_rework_clarification(delivery_id: &DeliveryId) -> ReworkClarificationFixture {
        let (journal, source_delivery, _candidate, _decision, _evidence_ref_ids) =
            rework_decision_fixture(delivery_id, 3);
        let history = derive_validated_rework_history(&source_delivery, &[])
            .expect("first rework fixture history");
        let reason = ReworkClarificationReason::RepeatedCriterionFailure;
        let clarification = derive_rework_clarification(&source_delivery, &history, reason)
            .expect("test-support repeated failure must be sealed");
        let transition = advance_rework(
            &source_delivery,
            advance_input(&source_delivery),
            ReworkDecision::Clarify(clarification),
        )
        .expect("repeated rework fixture must clarify");
        ReworkClarificationFixture {
            journal,
            source_delivery,
            transition,
            reason,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn rework_decision_fixture(
        delivery_id: &DeliveryId,
        max_rework_attempts: u64,
    ) -> (
        ReworkJournalFixture,
        Delivery,
        FrozenDeliveryCandidate,
        ReworkDecision,
        Vec<EvidenceId>,
    ) {
        let fixture = verdict_fixture_with_rework_limit(
            delivery_id,
            VerdictFixtureOutcome::Fail,
            max_rework_attempts,
        );
        let verdict_transition = compute_verdict_transition(
            &fixture.delivery,
            SubmitVerdictFacts {
                expected_revision: fixture.delivery.revision(),
                candidate: &fixture.candidate,
                verification: &fixture.verification,
                evidence: &fixture.evidence,
                produced_at_millis: 1_800_000_000_100,
            },
        )
        .expect("failing verdict fixture must compute");
        let failed = verdict_transition.delivery().clone();
        let attention = failed
            .snapshot()
            .attention_items
            .last()
            .expect("failing verdict must open Attention")
            .clone();
        let attention_transition = resolve_attention(
            &failed,
            ResolveAttentionInput {
                expected_revision: failed.revision(),
                attention_item_id: attention.id,
                stage_run_id: attention.stage_run_id.expect("linked Attention StageRun"),
                expected_context: attention.context,
                actor: attention.assigned_to.unwrap_or_else(|| "owner".into()),
                decision: AttentionDecision::Resolved,
                resolution: "start exact remediation".into(),
                now_millis: failed.snapshot().updated_at_millis + 1,
            },
        )
        .expect("failing Attention must enter Reworking");
        let reworking = attention_transition.clone().into_delivery();
        assert_eq!(reworking.snapshot().status, DeliveryStatus::Reworking);

        let verdict = reworking
            .snapshot()
            .verdict
            .as_ref()
            .expect("rework fixture verdict");
        let mut evidence_ref_ids = verdict
            .criteria
            .iter()
            .filter(|result| result.verdict == CriterionVerdict::Fail)
            .flat_map(|result| result.evidence_refs.iter().cloned())
            .collect::<Vec<_>>();
        evidence_ref_ids.sort_by(|left, right| left.0.cmp(&right.0));
        evidence_ref_ids.dedup();
        let delivery_task_id = fixture
            .candidate
            .producer_delivery_task_id()
            .expect("executor candidate task")
            .clone();
        let target = ReworkTargetFact {
            delivery_task_id: delivery_task_id.clone(),
            diagram_id: "diagram-main".into(),
            node_id: "node-api".into(),
            file_path: "src/invitation.rs".into(),
            hunk_sha256: "b".repeat(64),
            evidence_ref_ids: evidence_ref_ids.clone(),
        };
        let scope = CurrentReworkScope {
            candidate_ref: fixture.candidate.candidate_ref().into(),
            diff_sha256: fixture.candidate.diff_sha256().into(),
            targets: vec![target.clone()],
        };
        let annotation = PreciseReworkAnnotation {
            candidate_ref: scope.candidate_ref.clone(),
            diff_sha256: scope.diff_sha256.clone(),
            delivery_task_id,
            diagram_id: target.diagram_id,
            node_id: target.node_id,
            file_path: target.file_path,
            hunk_sha256: target.hunk_sha256,
            evidence_ref_ids: evidence_ref_ids.clone(),
        };
        let history =
            derive_validated_rework_history(&reworking, &[]).expect("first rework fixture history");
        let decision = decide_precise_rework(
            &reworking,
            &fixture.candidate,
            &scope,
            &[annotation],
            &history,
        )
        .expect("precise rework fixture decision");
        (
            ReworkJournalFixture {
                initial_delivery: fixture.delivery,
                verdict_transition,
                attention_transition,
            },
            reworking,
            fixture.candidate,
            decision,
            evidence_ref_ids,
        )
    }

    fn advance_input(delivery: &Delivery) -> AdvanceStageInput {
        AdvanceStageInput {
            expected_revision: delivery.revision(),
            product_session_id: ProductSessionId("psn_01J00000000000000000000000".into()),
            identities: NewStageIdentities {
                stage_run_id: StageRunId("run_01J00000000000000000000000".into()),
                execution_job_id: ExecutionJobId("job_01J00000000000000000000000".into()),
                session_binding_id: SessionBindingId("binding_01J00000000000000000000000".into()),
                attention_item_id: AttentionItemId("att_01J00000000000000000000000".into()),
            },
            review: None,
            previous_outcome: None,
            current_lease: None,
            rework_authorization: None,
            now_millis: delivery.snapshot().updated_at_millis + 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::freeze_rework_replacement_candidate_fixture;
    use super::*;
    use crate::application::{
        CoordinationErrorCode,
        stage::{
            AdvanceStageInput, NewStageIdentities, StageAdvanceEffect, advance, advance_rework,
            resume_active,
        },
    };
    use crate::domain::candidate::CandidateHunkFact;
    use crate::domain::candidate::test_support::{
        CandidateFixtureInput, freeze_facts, validated_git_snapshot,
        validated_git_snapshot_between, with_changed_hunks, with_foreign_terminal_workspace,
    };
    use crate::domain::{
        CandidatePathState, CriterionResult, DeliveryStatus, DeliveryTask, DeliveryTaskStatus,
        DeliveryVerdict, DeliveryVerdictId, EvidenceRef, EvidenceRefType, SessionBinding,
        SessionBindingId, StageRun, test_fixture,
    };
    use winwincode_domain::{
        AttentionItemId, CodexThreadId, ExecutionJobId, ProductSessionId, StageRunId,
        WorkerSessionId,
    };

    #[allow(clippy::too_many_lines)]
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
        let facts = validated_git_snapshot(
            &writer,
            &StageRunId("writer".into()),
            &SessionBindingId("writer-binding".into()),
            &"2".repeat(40),
            &"3".repeat(40),
            &"a".repeat(64),
            vec![super::super::CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("4".repeat(40)),
            }],
        );
        let candidate =
            super::super::freeze_delivery_candidate(&writer, &freeze_facts(&writer, facts))
                .expect("candidate");
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
            id: AttentionItemId("rework-attention".into()),
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

    fn empty_history(delivery: &Delivery) -> ValidatedReworkHistoryFact {
        derive_validated_rework_history(delivery, &[]).expect("empty pre-rework journal history")
    }

    fn map_delta_to_authorized_hunk(
        delta: ValidatedGitSnapshotFact,
        file_path: &str,
    ) -> ValidatedGitSnapshotFact {
        with_changed_hunks(
            delta,
            vec![CandidateHunkFact {
                file_path: file_path.into(),
                hunk_sha256: "f".repeat(64),
                source_hunk_sha256: Some("b".repeat(64)),
            }],
        )
    }

    fn authorization(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
        scope: &CurrentReworkScope,
        annotation: PreciseReworkAnnotation,
    ) -> ReworkAuthorization {
        let history = empty_history(delivery);
        let ReworkDecision::Start(authorization) =
            decide_precise_rework(delivery, candidate, scope, &[annotation], &history)
                .expect("rework authorization")
        else {
            panic!("expected rework authorization")
        };
        *authorization
    }

    fn resolve_rework_attention(delivery: Delivery) -> Delivery {
        let mut snapshot = delivery.into_snapshot();
        snapshot.status = DeliveryStatus::Reworking;
        snapshot.tasks[0].status = DeliveryTaskStatus::Failed;
        let attention = snapshot
            .attention_items
            .last_mut()
            .expect("rework Attention");
        attention.status = crate::domain::AttentionItemStatus::Dismissed;
        attention.resolution = Some("start exact remediation".into());
        attention.resolved_by = Some("owner".into());
        attention.resolved_at_millis = Some(snapshot.updated_at_millis + 1);
        snapshot.revision += 1;
        snapshot.updated_at_millis += 1;
        Delivery::try_from_snapshot(snapshot).expect("resolved rework Delivery")
    }

    fn dispatchable_rework() -> (Delivery, ReworkAuthorization) {
        let (delivery, candidate, scope, annotation) = current_failure();
        let delivery = resolve_rework_attention(delivery);
        let authorization = authorization(&delivery, &candidate, &scope, annotation);
        (delivery, authorization)
    }

    fn rework_advance_input(delivery: &Delivery) -> AdvanceStageInput {
        AdvanceStageInput {
            expected_revision: delivery.revision(),
            product_session_id: ProductSessionId("product-remediator-dispatch".into()),
            identities: NewStageIdentities {
                stage_run_id: StageRunId("stage-remediator-dispatch".into()),
                execution_job_id: ExecutionJobId("job-remediator-dispatch".into()),
                session_binding_id: SessionBindingId("binding-remediator-dispatch".into()),
                attention_item_id: AttentionItemId("attention-remediator-dispatch".into()),
            },
            review: None,
            previous_outcome: None,
            current_lease: None,
            rework_authorization: None,
            now_millis: delivery.snapshot().updated_at_millis + 1,
        }
    }

    fn append_remediator(
        delivery: Delivery,
        task_id: DeliveryTaskId,
        run_id: &str,
        binding_id: &str,
        attempt: u64,
        status: StageRunStatus,
    ) -> Delivery {
        let mut snapshot = delivery.into_snapshot();
        let started_at_millis = snapshot.updated_at_millis + 10;
        let finished_at_millis =
            (status == StageRunStatus::Succeeded).then_some(started_at_millis + 10);
        snapshot.stage_runs.push(StageRun {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: StageRunId(run_id.into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: Some(task_id.clone()),
            stage: super::super::DeliveryStage::Reworking,
            actor_type: StageRunActorType::Codex,
            role: "remediator".into(),
            status,
            attempt,
            started_at_millis,
            finished_at_millis,
        });
        snapshot.session_bindings.push(SessionBinding {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId(binding_id.into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: Some(task_id),
            stage_run_id: StageRunId(run_id.into()),
            product_session_id: ProductSessionId(format!("product-{run_id}")),
            execution_job_id: ExecutionJobId(format!("job-{run_id}")),
            worker_session_id: Some(WorkerSessionId(format!("worker-session-{run_id}"))),
            codex_thread_id: Some(CodexThreadId(format!("thread-{run_id}"))),
            bound_at_millis: started_at_millis + 1,
        });
        snapshot.status = if status == StageRunStatus::Running {
            DeliveryStatus::Reworking
        } else {
            DeliveryStatus::Verifying
        };
        snapshot.updated_at_millis = finished_at_millis.unwrap_or(started_at_millis + 1);
        snapshot.revision += 1;
        Delivery::try_from_snapshot(snapshot).expect("remediator Delivery")
    }

    #[test]
    fn freeze_rework_replacement_fixture_uses_authorized_old_to_new_delta() {
        let (delivery, previous, scope, annotation) = current_failure();
        let authorization = authorization(&delivery, &previous, &scope, annotation);
        let mut snapshot = delivery.into_snapshot();
        invalidate_candidate_authorization_for_writer_start(&mut snapshot);
        let cleared = Delivery::try_from_snapshot(snapshot).expect("cleared prior authority");
        let remediated = append_remediator(
            cleared,
            authorization.delivery_task_id().clone(),
            "remediator-fixture",
            "remediator-fixture-binding",
            authorization.next_attempt(),
            StageRunStatus::Succeeded,
        );
        let replacement = freeze_rework_replacement_candidate_fixture(
            &remediated,
            &authorization,
            &StageRunId("remediator-fixture".into()),
            &SessionBindingId("remediator-fixture-binding".into()),
            CandidateFixtureInput {
                base_commit_id: previous.candidate_commit_id().into(),
                base_tree_id: previous.candidate_tree_id().into(),
                candidate_commit_id: "5".repeat(40),
                candidate_tree_id: "6".repeat(40),
                diff_sha256: "c".repeat(64),
                changed_paths: vec![super::super::CandidatePathFact {
                    path: "src/invitation.rs".into(),
                    state: CandidatePathState::Present,
                    object_id: Some("7".repeat(40)),
                }],
                changed_hunks: vec![CandidateHunkFact {
                    file_path: "src/invitation.rs".into(),
                    hunk_sha256: "f".repeat(64),
                    source_hunk_sha256: Some("b".repeat(64)),
                }],
                artifact_ref: "artifact:fixture:remediator".into(),
                artifact_digest: Sha256Digest(format!("sha256:{}", "9".repeat(64))),
                terminal_event_sequence: 12,
            },
        );

        assert_eq!(replacement.producer_role(), "remediator");
        assert_eq!(
            replacement.base_commit_id(),
            remediated.snapshot().spec.base_revision
        );
        assert_eq!(replacement.candidate_commit_id(), "5".repeat(40));
    }

    #[test]
    fn rework_requires_precise_current_candidate_annotations() {
        let (delivery, candidate, scope, annotation) = current_failure();
        let history = empty_history(&delivery);
        let decision = decide_precise_rework(
            &delivery,
            &candidate,
            &scope,
            std::slice::from_ref(&annotation),
            &history,
        )
        .expect("authorization");
        assert!(matches!(decision, ReworkDecision::Start(_)));

        let mut stale = annotation;
        stale.hunk_sha256 = "0".repeat(64);
        assert!(decide_precise_rework(&delivery, &candidate, &scope, &[stale], &history,).is_err());
    }

    #[test]
    fn rework_evidence_must_be_cited_by_a_failing_criterion() {
        let (delivery, candidate, mut scope, mut annotation) = current_failure();
        let mut snapshot = delivery.into_snapshot();
        let mut unrelated = snapshot.evidence[0].clone();
        unrelated.id = EvidenceId("evidence-unrelated".into());
        unrelated.source_ref = "runtime_event:unrelated".into();
        snapshot.evidence.push(unrelated);
        let delivery = Delivery::try_from_snapshot(snapshot)
            .expect("current but unrelated Evidence remains canonical");
        let history = empty_history(&delivery);
        let unrelated_ids = vec![EvidenceId("evidence-unrelated".into())];
        scope.targets[0].evidence_ref_ids.clone_from(&unrelated_ids);
        annotation.evidence_ref_ids = unrelated_ids;

        decide_precise_rework(&delivery, &candidate, &scope, &[annotation], &history)
            .expect_err("unrelated Evidence cannot authorize a failure rework target");
    }

    #[test]
    fn rework_is_bounded_and_remediator_owned() {
        let (delivery, candidate, scope, annotation) = current_failure();
        let history = empty_history(&delivery);
        let ReworkDecision::Start(authorization) =
            decide_precise_rework(&delivery, &candidate, &scope, &[annotation], &history)
                .expect("decision")
        else {
            panic!("start")
        };
        assert_eq!(authorization.writer_actor(), StageRunActorType::Codex);
        assert_eq!(authorization.writer_role(), "remediator");
        assert_eq!(authorization.next_attempt(), 1);
    }

    #[test]
    fn reworking_advance_requires_and_preserves_the_exact_authorization() {
        let (delivery, authorization) = dispatchable_rework();

        let missing = advance(&delivery, rework_advance_input(&delivery))
            .expect_err("Reworking cannot choose a runnable task without authorization");
        assert_eq!(missing.code(), CoordinationErrorCode::AttentionRequired);

        let mut input = rework_advance_input(&delivery);
        input.rework_authorization = Some(Box::new(authorization.clone()));
        let result = advance(&delivery, input).expect("authorized rework starts");
        let run = result
            .delivery
            .snapshot()
            .stage_runs
            .last()
            .expect("remediator run");
        assert_eq!(
            run.delivery_task_id.as_ref(),
            Some(authorization.delivery_task_id())
        );
        assert_eq!(run.attempt, authorization.next_attempt());
        assert!(result.delivery.snapshot().evidence.is_empty());
        assert!(result.delivery.snapshot().verdict.is_none());
        let StageAdvanceEffect::Dispatch(intent) = result.effect else {
            panic!("authorized rework must dispatch")
        };
        assert_eq!(
            intent.delivery_task_id.as_ref(),
            Some(authorization.delivery_task_id())
        );
        assert_eq!(intent.attempt, authorization.next_attempt());
        assert_eq!(intent.rework_authorization(), Some(&authorization));
        assert!(resume_active(&result.delivery, result.delivery.revision()).is_err());
    }

    #[test]
    fn reworking_advance_rejects_stale_authorization_and_never_selects_another_failed_task() {
        let (delivery, mut authorization) = dispatchable_rework();
        let mut snapshot = delivery.into_snapshot();
        snapshot.tasks.insert(
            0,
            DeliveryTask {
                schema_version: super::super::DELIVERY_SCHEMA_VERSION,
                id: DeliveryTaskId("delivery-task-other-failure".into()),
                delivery_id: snapshot.id.clone(),
                title: "Other failure".into(),
                goal: "Must not be selected by runnable-task fallback".into(),
                acceptance_criterion_ids: vec![snapshot.spec.acceptance_criteria[0].id.clone()],
                blocked_by_task_ids: vec![],
                owner: None,
                status: DeliveryTaskStatus::Failed,
            },
        );
        let delivery = Delivery::try_from_snapshot(snapshot).expect("two failed tasks");

        let mut input = rework_advance_input(&delivery);
        input.rework_authorization = Some(Box::new(authorization.clone()));
        let result = advance(&delivery, input).expect("exact authorized task starts");
        let run = result
            .delivery
            .snapshot()
            .stage_runs
            .last()
            .expect("rework run");
        assert_eq!(
            run.delivery_task_id.as_ref(),
            Some(authorization.delivery_task_id())
        );
        assert_eq!(
            result.delivery.snapshot().tasks[0].status,
            DeliveryTaskStatus::Failed
        );

        authorization.next_attempt += 1;
        let mut stale_input = rework_advance_input(&delivery);
        stale_input.identities.stage_run_id = StageRunId("stage-remediator-stale".into());
        stale_input.rework_authorization = Some(Box::new(authorization));
        let stale = advance(&delivery, stale_input)
            .expect_err("caller cannot raise the Delivery-wide attempt");
        assert_eq!(stale.code(), CoordinationErrorCode::Conflict);

        let (delivery, authorization) = dispatchable_rework();
        let mut changed_session = delivery.clone().into_snapshot();
        let source_binding = changed_session
            .session_bindings
            .iter_mut()
            .find(|binding| binding.id.0 == "writer-binding")
            .expect("source binding");
        source_binding.product_session_id = ProductSessionId("writer-product-rebound".into());
        let changed_session =
            Delivery::try_from_snapshot(changed_session).expect("changed source SessionBinding");
        let mut input = rework_advance_input(&changed_session);
        input.rework_authorization = Some(Box::new(authorization.clone()));
        assert!(
            advance(&changed_session, input).is_err(),
            "old authorization cannot survive a source SessionBinding change"
        );

        let mut changed_terminal = delivery.into_snapshot();
        let source_run = changed_terminal
            .stage_runs
            .iter_mut()
            .find(|run| run.id.0 == "writer")
            .expect("source run");
        source_run.finished_at_millis = source_run.finished_at_millis.map(|value| value + 1);
        let changed_terminal =
            Delivery::try_from_snapshot(changed_terminal).expect("changed terminal time");
        let mut input = rework_advance_input(&changed_terminal);
        input.rework_authorization = Some(Box::new(authorization));
        assert!(
            advance(&changed_terminal, input).is_err(),
            "old authorization cannot survive a source terminal change"
        );
    }

    #[test]
    fn old_candidate_evidence_and_verdict_cannot_authorize_after_writer_starts() {
        let (delivery, candidate, scope, annotation) = current_failure();
        let authorization = authorization(&delivery, &candidate, &scope, annotation);
        assert!(authorization.requires_full_reverification());

        let mut snapshot = delivery.into_snapshot();
        invalidate_candidate_authorization_for_writer_start(&mut snapshot);
        let invalidated = Delivery::try_from_snapshot(snapshot).expect("invalidated current facts");
        let with_writer = append_remediator(
            invalidated,
            authorization.delivery_task_id().clone(),
            "remediator-running",
            "remediator-running-binding",
            authorization.next_attempt(),
            StageRunStatus::Running,
        );
        assert!(with_writer.snapshot().evidence.is_empty());
        assert!(with_writer.snapshot().verdict.is_none());
        assert!(assert_frozen_candidate_current(&with_writer, &candidate).is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn replacement_candidate_requires_sealed_snapshot_within_approved_scope() {
        let (delivery, previous, scope, annotation) = current_failure();
        let authorization = authorization(&delivery, &previous, &scope, annotation);
        authorization
            .validate_for_dispatch(&delivery)
            .expect("current authorization is dispatchable");
        let mut snapshot = delivery.clone().into_snapshot();
        invalidate_candidate_authorization_for_writer_start(&mut snapshot);
        let cleared = Delivery::try_from_snapshot(snapshot).expect("cleared prior authority");
        let remediated = append_remediator(
            cleared,
            authorization.delivery_task_id().clone(),
            "remediator-finished",
            "remediator-finished-binding",
            authorization.next_attempt(),
            StageRunStatus::Succeeded,
        );
        let replacement_snapshot = validated_git_snapshot(
            &remediated,
            &StageRunId("remediator-finished".into()),
            &SessionBindingId("remediator-finished-binding".into()),
            &"5".repeat(40),
            &"6".repeat(40),
            &"c".repeat(64),
            vec![super::super::CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("7".repeat(40)),
            }],
        );
        let replacement_facts = freeze_facts(&remediated, replacement_snapshot);
        let authorized_delta = map_delta_to_authorized_hunk(
            validated_git_snapshot_between(
                &remediated,
                &StageRunId("remediator-finished".into()),
                &SessionBindingId("remediator-finished-binding".into()),
                previous.candidate_commit_id(),
                previous.candidate_tree_id(),
                &"5".repeat(40),
                &"6".repeat(40),
                &"d".repeat(64),
                vec![super::super::CandidatePathFact {
                    path: "src/invitation.rs".into(),
                    state: CandidatePathState::Present,
                    object_id: Some("7".repeat(40)),
                }],
            ),
            "src/invitation.rs",
        );
        let replacement = freeze_rework_replacement_candidate(
            &remediated,
            &authorization,
            &replacement_facts,
            &authorized_delta,
        )
        .expect("authorized sealed replacement delta");
        assert_eq!(replacement.producer_role(), "remediator");

        let out_of_scope_snapshot = validated_git_snapshot(
            &remediated,
            &StageRunId("remediator-finished".into()),
            &SessionBindingId("remediator-finished-binding".into()),
            &"8".repeat(40),
            &"9".repeat(40),
            &"e".repeat(64),
            vec![super::super::CandidatePathFact {
                path: "src/foreign.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("a".repeat(40)),
            }],
        );
        let out_of_scope_facts = freeze_facts(&remediated, out_of_scope_snapshot);
        let out_of_scope_delta = map_delta_to_authorized_hunk(
            validated_git_snapshot_between(
                &remediated,
                &StageRunId("remediator-finished".into()),
                &SessionBindingId("remediator-finished-binding".into()),
                previous.candidate_commit_id(),
                previous.candidate_tree_id(),
                &"8".repeat(40),
                &"9".repeat(40),
                &"e".repeat(64),
                vec![super::super::CandidatePathFact {
                    path: "src/foreign.rs".into(),
                    state: CandidatePathState::Present,
                    object_id: Some("a".repeat(40)),
                }],
            ),
            "src/foreign.rs",
        );
        freeze_rework_replacement_candidate(
            &remediated,
            &authorization,
            &out_of_scope_facts,
            &out_of_scope_delta,
        )
        .expect_err("out-of-scope remediator output cannot become a frozen candidate");

        let mut changed_spec_snapshot = delivery.into_snapshot();
        invalidate_candidate_authorization_for_writer_start(&mut changed_spec_snapshot);
        changed_spec_snapshot.spec.revision += 1;
        changed_spec_snapshot.revision += 1;
        let changed_spec = Delivery::try_from_snapshot(changed_spec_snapshot)
            .expect("revised DeliverySpec after the old authorization");
        let changed_spec_remediated = append_remediator(
            changed_spec,
            authorization.delivery_task_id().clone(),
            "remediator-revised-spec",
            "remediator-revised-spec-binding",
            authorization.next_attempt(),
            StageRunStatus::Succeeded,
        );
        let changed_spec_candidate_snapshot = validated_git_snapshot(
            &changed_spec_remediated,
            &StageRunId("remediator-revised-spec".into()),
            &SessionBindingId("remediator-revised-spec-binding".into()),
            &"5".repeat(40),
            &"6".repeat(40),
            &"c".repeat(64),
            vec![super::super::CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("7".repeat(40)),
            }],
        );
        let changed_spec_facts =
            freeze_facts(&changed_spec_remediated, changed_spec_candidate_snapshot);
        let changed_spec_delta = map_delta_to_authorized_hunk(
            validated_git_snapshot_between(
                &changed_spec_remediated,
                &StageRunId("remediator-revised-spec".into()),
                &SessionBindingId("remediator-revised-spec-binding".into()),
                previous.candidate_commit_id(),
                previous.candidate_tree_id(),
                &"5".repeat(40),
                &"6".repeat(40),
                &"d".repeat(64),
                vec![super::super::CandidatePathFact {
                    path: "src/invitation.rs".into(),
                    state: CandidatePathState::Present,
                    object_id: Some("7".repeat(40)),
                }],
            ),
            "src/invitation.rs",
        );
        freeze_rework_replacement_candidate(
            &changed_spec_remediated,
            &authorization,
            &changed_spec_facts,
            &changed_spec_delta,
        )
        .expect_err("a replacement authorization is scoped to its original DeliverySpec");

        let mut foreign_binding_snapshot = remediated.clone().into_snapshot();
        let foreign_binding = foreign_binding_snapshot
            .session_bindings
            .iter_mut()
            .find(|binding| binding.id.0 == "remediator-finished-binding")
            .expect("remediator binding");
        foreign_binding.id = SessionBindingId("remediator-foreign-binding".into());
        foreign_binding.product_session_id = ProductSessionId("product-remediator-foreign".into());
        let foreign_binding_delivery = Delivery::try_from_snapshot(foreign_binding_snapshot)
            .expect("foreign sealed delta fixture");
        let foreign_binding_delta = map_delta_to_authorized_hunk(
            validated_git_snapshot_between(
                &foreign_binding_delivery,
                &StageRunId("remediator-finished".into()),
                &SessionBindingId("remediator-foreign-binding".into()),
                previous.candidate_commit_id(),
                previous.candidate_tree_id(),
                &"5".repeat(40),
                &"6".repeat(40),
                &"d".repeat(64),
                vec![super::super::CandidatePathFact {
                    path: "src/invitation.rs".into(),
                    state: CandidatePathState::Present,
                    object_id: Some("7".repeat(40)),
                }],
            ),
            "src/invitation.rs",
        );
        assert!(
            freeze_rework_replacement_candidate(
                &remediated,
                &authorization,
                &replacement_facts,
                &foreign_binding_delta,
            )
            .is_err()
        );

        let foreign_terminal_delta = with_foreign_terminal_workspace(authorized_delta.clone());
        freeze_rework_replacement_candidate(
            &remediated,
            &authorization,
            &replacement_facts,
            &foreign_terminal_delta,
        )
        .expect_err(
            "replacement delta rejects foreign lease, fence, Worker, artifact, and terminal boundary",
        );

        let mut stale_attempt_snapshot = remediated.clone().into_snapshot();
        let producer_index = stale_attempt_snapshot
            .stage_runs
            .iter()
            .position(|run| run.id.0 == "remediator-finished")
            .expect("replacement producer");
        stale_attempt_snapshot.stage_runs.insert(
            producer_index,
            StageRun {
                schema_version: super::super::DELIVERY_SCHEMA_VERSION,
                id: StageRunId("remediator-unrecorded-prior".into()),
                delivery_id: stale_attempt_snapshot.id.clone(),
                delivery_task_id: Some(authorization.delivery_task_id().clone()),
                stage: super::super::DeliveryStage::Reworking,
                actor_type: StageRunActorType::Codex,
                role: "remediator".into(),
                status: StageRunStatus::Succeeded,
                attempt: 1,
                started_at_millis: 1_800_000_000_024,
                finished_at_millis: Some(1_800_000_000_025),
            },
        );
        stale_attempt_snapshot
            .session_bindings
            .push(SessionBinding {
                schema_version: super::super::DELIVERY_SCHEMA_VERSION,
                id: SessionBindingId("binding-remediator-unrecorded-prior".into()),
                delivery_id: stale_attempt_snapshot.id.clone(),
                delivery_task_id: Some(authorization.delivery_task_id().clone()),
                stage_run_id: StageRunId("remediator-unrecorded-prior".into()),
                product_session_id: ProductSessionId("product-remediator-unrecorded-prior".into()),
                execution_job_id: ExecutionJobId("job-remediator-unrecorded-prior".into()),
                worker_session_id: Some(WorkerSessionId(
                    "worker-session-remediator-unrecorded-prior".into(),
                )),
                codex_thread_id: Some(CodexThreadId("thread-remediator-unrecorded-prior".into())),
                bound_at_millis: 1_800_000_000_024,
            });
        let stale_attempt = Delivery::try_from_snapshot(stale_attempt_snapshot)
            .expect("global rework history changed after authorization");
        assert!(
            freeze_rework_replacement_candidate(
                &stale_attempt,
                &authorization,
                &replacement_facts,
                &authorized_delta,
            )
            .is_err()
        );

        let expanded_delta = map_delta_to_authorized_hunk(
            validated_git_snapshot_between(
                &remediated,
                &StageRunId("remediator-finished".into()),
                &SessionBindingId("remediator-finished-binding".into()),
                previous.candidate_commit_id(),
                previous.candidate_tree_id(),
                &"5".repeat(40),
                &"6".repeat(40),
                &"e".repeat(64),
                vec![super::super::CandidatePathFact {
                    path: "src/foreign.rs".into(),
                    state: CandidatePathState::Present,
                    object_id: Some("8".repeat(40)),
                }],
            ),
            "src/foreign.rs",
        );
        assert!(
            freeze_rework_replacement_candidate(
                &remediated,
                &authorization,
                &replacement_facts,
                &expanded_delta,
            )
            .is_err()
        );
    }

    #[test]
    fn combined_attention_actions_use_safest_transition_independent_of_resolution_order() {
        use VerdictAttentionAction::{ClarifyDefinition, RetryVerification, StartRework};

        let orders = [
            [ClarifyDefinition, StartRework, RetryVerification],
            [RetryVerification, ClarifyDefinition, StartRework],
            [StartRework, RetryVerification, ClarifyDefinition],
        ];
        for actions in orders {
            assert_eq!(
                safest_attention_transition(&actions),
                DeliveryStatus::Clarifying
            );
        }
        assert_eq!(
            safest_attention_transition(&[RetryVerification, StartRework]),
            DeliveryStatus::Reworking
        );
    }

    #[test]
    fn rework_attempt_uses_total_delivery_rework_history() {
        let (delivery, _, _, _) = current_failure();
        let mut snapshot = delivery.into_snapshot();
        snapshot.spec.max_rework_attempts = 4;
        let second_task = DeliveryTask {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: DeliveryTaskId("delivery-task-secondary".into()),
            delivery_id: snapshot.id.clone(),
            title: "Secondary task".into(),
            goal: "Prove the attempt sequence is Delivery-wide".into(),
            acceptance_criterion_ids: vec![snapshot.spec.acceptance_criteria[0].id.clone()],
            blocked_by_task_ids: vec![],
            owner: None,
            status: DeliveryTaskStatus::Pending,
        };
        snapshot.tasks.push(second_task.clone());
        let delivery = Delivery::try_from_snapshot(snapshot).expect("two tasks");
        let delivery = append_remediator(
            delivery,
            DeliveryTaskId("delivery-task-api".into()),
            "remediator-task-one",
            "binding-remediator-task-one",
            1,
            StageRunStatus::Succeeded,
        );
        let delivery = append_remediator(
            delivery,
            second_task.id,
            "remediator-task-two",
            "binding-remediator-task-two",
            2,
            StageRunStatus::Succeeded,
        );
        assert_eq!(next_rework_attempt(&delivery), 3);
    }

    #[test]
    fn repeated_or_exhausted_failure_requires_clarification() {
        let (mut delivery, candidate, scope, annotation) = current_failure();
        let required = delivery.snapshot().spec.acceptance_criteria[0].id.clone();
        let mut snapshot = delivery.into_snapshot();
        snapshot.spec.max_rework_attempts = 0;
        delivery = Delivery::try_from_snapshot(snapshot).expect("zero limit");
        delivery = resolve_rework_attention(delivery);
        let history = empty_history(&delivery);
        let exhausted =
            decide_precise_rework(&delivery, &candidate, &scope, &[annotation], &history)
                .expect("decision");
        assert!(matches!(
            &exhausted,
            ReworkDecision::Clarify(clarification)
                if clarification.reason() == ReworkClarificationReason::AttemptLimitExhausted
        ));
        let prior_run_count = delivery.snapshot().stage_runs.len();
        let prior_binding_count = delivery.snapshot().session_bindings.len();
        let clarified = advance_rework(&delivery, rework_advance_input(&delivery), exhausted)
            .expect("exhausted rework enters clarification");
        assert_eq!(
            clarified.delivery.snapshot().status,
            DeliveryStatus::Clarifying
        );
        assert_eq!(
            clarified.delivery.snapshot().stage_runs.len(),
            prior_run_count
        );
        assert_eq!(
            clarified.delivery.snapshot().session_bindings.len(),
            prior_binding_count
        );
        assert_eq!(
            clarified.effect,
            StageAdvanceEffect::Clarify(ReworkClarificationReason::AttemptLimitExhausted)
        );

        let (prior_delivery, _, _, _) = current_failure();
        let prior_snapshot = prior_delivery.snapshot().clone();
        let mut repeated_snapshot = append_remediator(
            prior_delivery,
            DeliveryTaskId("delivery-task-api".into()),
            "remediator-prior-failure",
            "binding-remediator-prior-failure",
            1,
            StageRunStatus::Succeeded,
        )
        .into_snapshot();
        let repeated_candidate_ref = "candidate-after-rework";
        for evidence in &mut repeated_snapshot.evidence {
            evidence.candidate_ref = repeated_candidate_ref.into();
        }
        let verdict = repeated_snapshot.verdict.as_mut().expect("current verdict");
        verdict.candidate_ref = repeated_candidate_ref.into();
        for result in &mut verdict.criteria {
            result.candidate_ref = repeated_candidate_ref.into();
        }
        let repeated_delivery = Delivery::try_from_snapshot(repeated_snapshot)
            .expect("current failure after one prior remediator");
        assert!(derive_validated_rework_history(&repeated_delivery, &[]).is_err());
        let history = derive_validated_rework_history(
            &repeated_delivery,
            std::slice::from_ref(&prior_snapshot),
        )
        .expect("append-only prior failure history");
        let failures = HashSet::from([required.0.as_str()]);
        let repeated_reason = clarification_reason(1, 3, &failures, &history);
        assert_eq!(
            repeated_reason,
            Some(ReworkClarificationReason::RepeatedCriterionFailure)
        );

        let (repeated_transition, _, _, _) = current_failure();
        let repeated_transition = resolve_rework_attention(repeated_transition);
        let repeated_transition_history = empty_history(&repeated_transition);
        let repeated_clarification = derive_rework_clarification(
            &repeated_transition,
            &repeated_transition_history,
            repeated_reason.expect("repeated failure"),
        )
        .expect("sealed repeated clarification");
        let repeated = advance_rework(
            &repeated_transition,
            rework_advance_input(&repeated_transition),
            ReworkDecision::Clarify(repeated_clarification),
        )
        .expect("repeated failure enters clarification");
        assert_eq!(
            repeated.delivery.snapshot().status,
            DeliveryStatus::Clarifying
        );
        assert!(matches!(
            repeated.effect,
            StageAdvanceEffect::Clarify(ReworkClarificationReason::RepeatedCriterionFailure)
        ));
    }
}
