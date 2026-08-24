// SPDX-License-Identifier: Apache-2.0

//! Atomic application transition for one current computed Delivery verdict.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{AttentionItemId, DeliveryTaskId, EvidenceId, StageRunId};

use crate::domain::{
    AttentionItem, AttentionItemStatus, AttentionItemType, AttentionOption, CriterionResult,
    CriterionVerdict, DELIVERY_SCHEMA_VERSION, Delivery, DeliverySnapshot, DeliveryStatus,
    DeliveryTaskStatus, DeliveryVerdict, DeliveryVerdictId, EvidenceRef, FrozenDeliveryCandidate,
    evidence::ResolvedDeliveryEvidence,
    rework::VerdictAttentionAction,
    verification::{IndependentVerification, VerificationRole},
};

use super::{CoordinationError, CoordinationErrorCode, require_mutation_time};

const VERDICT_ATTENTION_PROTOCOL: &str = "winwincode.delivery-verdict-attention.v1";

/// Sealed authoritative facts accepted by the verdict application service.
#[derive(Debug, Clone, Copy)]
pub struct SubmitVerdictFacts<'facts> {
    pub expected_revision: u64,
    pub candidate: &'facts FrozenDeliveryCandidate,
    pub verification: &'facts IndependentVerification,
    pub evidence: &'facts [ResolvedDeliveryEvidence],
    pub produced_at_millis: u64,
}

/// One constructor-derived Delivery transition accepted by persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputedVerdictTransition {
    source_revision: u64,
    source_digest: String,
    candidate_ref: String,
    delivery: Delivery,
    event: DeliveryVerdictSubmittedEvent,
}

impl ComputedVerdictTransition {
    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    pub fn event(&self) -> &DeliveryVerdictSubmittedEvent {
        &self.event
    }

    pub(crate) fn validate_source(&self, source: &Delivery) -> Result<(), CoordinationError> {
        if source.revision() != self.source_revision
            || source.id() != self.delivery.id()
            || delivery_digest(source)? != self.source_digest
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::RevisionConflict,
                "computed verdict transition no longer matches the durable Delivery source",
            ));
        }
        validate_transition_delta(source, self)
    }
}

/// Immutable outbox projection produced from the same computed transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryVerdictSubmittedEvent {
    pub schema_version: u8,
    pub delivery_id: winwincode_domain::DeliveryId,
    pub delivery_revision: u64,
    pub candidate_ref: String,
    pub evidence: Vec<EvidenceRef>,
    pub verdict: DeliveryVerdict,
    pub attention_items: Vec<AttentionItem>,
    pub task_statuses: Vec<DeliveryTaskStatusFact>,
    pub status: DeliveryStatus,
    pub produced_at_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryTaskStatusFact {
    pub delivery_task_id: DeliveryTaskId,
    pub status: DeliveryTaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DerivedVerdictAttentionAction {
    StartRework,
    RetryVerification,
    CompleteVerification,
    ResolveVerificationConflict,
    ClarifyDefinition,
}

impl DerivedVerdictAttentionAction {
    const fn id(self) -> &'static str {
        match self {
            Self::StartRework => "start-rework",
            Self::RetryVerification => "retry-verification",
            Self::CompleteVerification => "complete-verification",
            Self::ResolveVerificationConflict => "resolve-verification-conflict",
            Self::ClarifyDefinition => "clarify-definition",
        }
    }

    const fn transition_action(self) -> VerdictAttentionAction {
        match self {
            Self::StartRework => VerdictAttentionAction::StartRework,
            Self::ClarifyDefinition => VerdictAttentionAction::ClarifyDefinition,
            Self::RetryVerification
            | Self::CompleteVerification
            | Self::ResolveVerificationConflict => VerdictAttentionAction::RetryVerification,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerdictAttentionContext {
    protocol: String,
    verdict_id: DeliveryVerdictId,
    candidate_ref: String,
    stage_run_id: StageRunId,
    action: DerivedVerdictAttentionAction,
    criterion_result_id: Option<crate::domain::CriterionResultId>,
    criterion_id: Option<crate::domain::AcceptanceCriterionId>,
    evidence_ref_ids: Vec<EvidenceId>,
    unresolved_findings: Vec<String>,
    rework_attempts_used: u64,
    rework_attempts_limit: u64,
}

/// Recomputes Evidence, Verdict, Attention, task state, and Delivery status.
///
/// # Errors
///
/// Returns a stable coordination error for a stale revision or any rejected
/// authoritative fact. No partial snapshot is returned.
pub fn compute_verdict_transition(
    delivery: &Delivery,
    facts: SubmitVerdictFacts<'_>,
) -> Result<ComputedVerdictTransition, CoordinationError> {
    if delivery.revision() != facts.expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before verdict computation",
        ));
    }
    require_mutation_time(delivery, facts.produced_at_millis)?;
    if delivery.snapshot().status != DeliveryStatus::Verifying
        || delivery.snapshot().verdict.is_some()
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "a Delivery verdict can be computed only once from the verifying state",
        ));
    }
    if delivery
        .snapshot()
        .attention_items
        .iter()
        .any(|item| item.blocking && item.status == AttentionItemStatus::Open)
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::AttentionRequired,
            "open blocking Attention must be resolved before verdict computation",
        ));
    }

    let computed = crate::domain::compute_delivery_verdict(
        delivery,
        facts.candidate,
        facts.verification,
        facts.evidence,
        facts.produced_at_millis,
    )
    .map_err(|error| CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string()))?;
    let (computed_evidence, verdict, _) = computed.into_parts();
    let verification_stage_run_id =
        attention_stage_run_id(delivery, facts.candidate, facts.verification);

    let mut snapshot = delivery.snapshot().clone();
    append_new_evidence(&mut snapshot, &computed_evidence)?;
    snapshot.verdict = Some(verdict.clone());
    update_task_statuses(&mut snapshot, &verdict);
    let new_attention = if verdict.status == CriterionVerdict::Pass {
        Vec::new()
    } else {
        derive_verdict_attention(
            &snapshot,
            &verdict,
            &verification_stage_run_id,
            facts.produced_at_millis,
        )?
    };
    if new_attention.iter().any(|item| {
        snapshot
            .attention_items
            .iter()
            .any(|stored| stored.id == item.id)
    }) {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "derived Attention identity already exists in the current Delivery",
        ));
    }
    if verdict.status != CriterionVerdict::Pass && new_attention.is_empty() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::AttentionRequired,
            "a non-passing verdict must derive a complete blocking Attention set",
        ));
    }
    snapshot.attention_items.extend(new_attention.clone());
    snapshot.status = if verdict.status == CriterionVerdict::Pass {
        DeliveryStatus::ReadyToDeliver
    } else {
        DeliveryStatus::NeedsAttention
    };
    snapshot.revision = delivery.revision().checked_add(1).ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "Delivery revision cannot advance",
        )
    })?;
    snapshot.updated_at_millis = facts.produced_at_millis;
    let next = Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
    })?;
    let event = event_from_transition(&next, computed_evidence, verdict, new_attention);
    let transition = ComputedVerdictTransition {
        source_revision: delivery.revision(),
        source_digest: delivery_digest(delivery)?,
        candidate_ref: facts.candidate.candidate_ref().into(),
        delivery: next,
        event,
    };
    transition.validate_source(delivery)?;
    Ok(transition)
}

fn append_new_evidence(
    snapshot: &mut DeliverySnapshot,
    computed: &[EvidenceRef],
) -> Result<(), CoordinationError> {
    for evidence in computed {
        match snapshot
            .evidence
            .iter()
            .find(|stored| stored.id == evidence.id)
        {
            Some(stored) if stored == evidence => {}
            Some(_) => {
                return Err(CoordinationError::new(
                    CoordinationErrorCode::Conflict,
                    "computed Evidence identity collides with another canonical source",
                ));
            }
            None => snapshot.evidence.push(evidence.clone()),
        }
    }
    Ok(())
}

fn update_task_statuses(snapshot: &mut DeliverySnapshot, verdict: &DeliveryVerdict) {
    if verdict.status == CriterionVerdict::Pass {
        for task in &mut snapshot.tasks {
            task.status = DeliveryTaskStatus::Completed;
        }
        return;
    }
    let failed = verdict
        .criteria
        .iter()
        .filter(|result| result.verdict == CriterionVerdict::Fail)
        .map(|result| &result.criterion_id)
        .collect::<Vec<_>>();
    for task in &mut snapshot.tasks {
        if task
            .acceptance_criterion_ids
            .iter()
            .any(|criterion_id| failed.contains(&criterion_id))
        {
            task.status = DeliveryTaskStatus::Failed;
        }
    }
}

fn attention_stage_run_id(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    verification: &IndependentVerification,
) -> StageRunId {
    [VerificationRole::Verifier, VerificationRole::Reviewer]
        .into_iter()
        .find_map(|role| {
            verification
                .settlements()
                .iter()
                .find(|settlement| settlement.role() == role)
                .and_then(|settlement| settlement.assignment())
                .map(|assignment| assignment.stage_run_id().clone())
        })
        .or_else(|| {
            delivery
                .snapshot()
                .stage_runs
                .iter()
                .filter(|run| run.stage == crate::domain::DeliveryStage::Verifying)
                .max_by(|left, right| {
                    (left.attempt, left.role.as_str(), left.id.0.as_str()).cmp(&(
                        right.attempt,
                        right.role.as_str(),
                        right.id.0.as_str(),
                    ))
                })
                .map(|run| run.id.clone())
        })
        .unwrap_or_else(|| candidate.producer_stage_run_id().clone())
}

fn derive_verdict_attention(
    snapshot: &DeliverySnapshot,
    verdict: &DeliveryVerdict,
    stage_run_id: &StageRunId,
    created_at_millis: u64,
) -> Result<Vec<AttentionItem>, CoordinationError> {
    let mut items = Vec::new();
    for result in verdict.criteria.iter().filter(|result| {
        snapshot
            .spec
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.required && criterion.id == result.criterion_id)
            && result.verdict != CriterionVerdict::Pass
    }) {
        let findings = criterion_findings(verdict, result);
        let action = attention_action(snapshot, result, &findings);
        items.push(attention_item(
            snapshot,
            verdict,
            stage_run_id,
            Some(result),
            findings,
            action,
            created_at_millis,
        )?);
    }
    for finding in verdict
        .unresolved_findings
        .iter()
        .filter(|finding| finding.starts_with("unscoped-finding:"))
    {
        items.push(attention_item(
            snapshot,
            verdict,
            stage_run_id,
            None,
            vec![finding.clone()],
            DerivedVerdictAttentionAction::ClarifyDefinition,
            created_at_millis,
        )?);
    }
    if items.is_empty() && verdict.status != CriterionVerdict::Pass {
        items.push(attention_item(
            snapshot,
            verdict,
            stage_run_id,
            None,
            verdict.unresolved_findings.clone(),
            DerivedVerdictAttentionAction::CompleteVerification,
            created_at_millis,
        )?);
    }
    items.sort_by(|left, right| left.id.0.cmp(&right.id.0));
    if items.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "derived Attention identity is duplicated",
        ));
    }
    Ok(items)
}

fn criterion_findings(verdict: &DeliveryVerdict, result: &CriterionResult) -> Vec<String> {
    let needle = format!(":{}:", result.criterion_id.0);
    verdict
        .unresolved_findings
        .iter()
        .filter(|finding| finding.contains(&needle))
        .cloned()
        .collect()
}

fn attention_action(
    snapshot: &DeliverySnapshot,
    result: &CriterionResult,
    findings: &[String],
) -> DerivedVerdictAttentionAction {
    match result.verdict {
        CriterionVerdict::Fail => {
            let attempts = snapshot
                .stage_runs
                .iter()
                .filter(|run| run.stage == crate::domain::DeliveryStage::Reworking)
                .count() as u64;
            if attempts >= snapshot.spec.max_rework_attempts {
                DerivedVerdictAttentionAction::ClarifyDefinition
            } else {
                DerivedVerdictAttentionAction::StartRework
            }
        }
        CriterionVerdict::InfraError => DerivedVerdictAttentionAction::RetryVerification,
        CriterionVerdict::Inconclusive
            if findings
                .iter()
                .any(|finding| finding.starts_with("contradiction:")) =>
        {
            DerivedVerdictAttentionAction::ResolveVerificationConflict
        }
        CriterionVerdict::Inconclusive => DerivedVerdictAttentionAction::CompleteVerification,
        CriterionVerdict::Pass => DerivedVerdictAttentionAction::ClarifyDefinition,
    }
}

fn attention_item(
    snapshot: &DeliverySnapshot,
    verdict: &DeliveryVerdict,
    stage_run_id: &StageRunId,
    result: Option<&CriterionResult>,
    unresolved_findings: Vec<String>,
    action: DerivedVerdictAttentionAction,
    created_at_millis: u64,
) -> Result<AttentionItem, CoordinationError> {
    let context = VerdictAttentionContext {
        protocol: VERDICT_ATTENTION_PROTOCOL.into(),
        verdict_id: verdict.id.clone(),
        candidate_ref: verdict.candidate_ref.clone(),
        stage_run_id: stage_run_id.clone(),
        action,
        criterion_result_id: result.map(|result| result.id.clone()),
        criterion_id: result.map(|result| result.criterion_id.clone()),
        evidence_ref_ids: result.map_or_else(Vec::new, |result| result.evidence_refs.clone()),
        unresolved_findings,
        rework_attempts_used: snapshot
            .stage_runs
            .iter()
            .filter(|run| run.stage == crate::domain::DeliveryStage::Reworking)
            .count() as u64,
        rework_attempts_limit: snapshot.spec.max_rework_attempts,
    };
    let encoded_context = serde_json::to_string(&context).map_err(|error| {
        CoordinationError::new(
            CoordinationErrorCode::Conflict,
            format!("derived Attention context cannot be encoded: {error}"),
        )
    })?;
    let digest = Sha256::digest(encoded_context.as_bytes());
    let (item_type, title, label, description) = attention_copy(action);
    Ok(AttentionItem {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: AttentionItemId(format!("attention:sha256:{digest:x}")),
        delivery_id: snapshot.id.clone(),
        delivery_spec_id: snapshot.spec.id.clone(),
        stage_run_id: Some(stage_run_id.clone()),
        item_type,
        title: title.into(),
        context: encoded_context,
        options: vec![AttentionOption {
            schema_version: DELIVERY_SCHEMA_VERSION,
            id: action.id().into(),
            label: label.into(),
            description: description.into(),
        }],
        assigned_to: None,
        blocking: true,
        status: AttentionItemStatus::Open,
        resolution: None,
        resolved_by: None,
        created_at_millis,
        resolved_at_millis: None,
    })
}

fn attention_copy(
    action: DerivedVerdictAttentionAction,
) -> (AttentionItemType, &'static str, &'static str, &'static str) {
    match action {
        DerivedVerdictAttentionAction::StartRework => (
            AttentionItemType::DecisionRequired,
            "Acceptance criterion requires bounded rework",
            "Start bounded rework",
            "Approve a remediator run limited to the cited task, evidence, file, and hunk scope.",
        ),
        DerivedVerdictAttentionAction::RetryVerification => (
            AttentionItemType::VerificationBlocked,
            "Verification infrastructure must be retried",
            "Retry verification",
            "Verify the unchanged candidate again with a current accepted Worker outcome.",
        ),
        DerivedVerdictAttentionAction::CompleteVerification => (
            AttentionItemType::VerificationBlocked,
            "Verification evidence is incomplete",
            "Complete verification",
            "Collect current direct evidence from every required independent role.",
        ),
        DerivedVerdictAttentionAction::ResolveVerificationConflict => (
            AttentionItemType::DecisionRequired,
            "Independent verification findings conflict",
            "Resolve and reverify",
            "Resolve the cited disagreement before verifying the unchanged candidate again.",
        ),
        DerivedVerdictAttentionAction::ClarifyDefinition => (
            AttentionItemType::ScopeChange,
            "Delivery definition requires clarification",
            "Clarify delivery scope",
            "Review and approve a revised DeliverySpec before more code execution.",
        ),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the complete derived Attention set is validated in one fail-closed pass"
)]
pub(crate) fn current_verdict_attention_actions(
    delivery: &Delivery,
    item: &AttentionItem,
) -> Result<Option<Vec<VerdictAttentionAction>>, CoordinationError> {
    let claimed_protocol = serde_json::from_str::<serde_json::Value>(&item.context)
        .ok()
        .and_then(|context| {
            context
                .get("protocol")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some(VERDICT_ATTENTION_PROTOCOL);
    let Some(verdict) = delivery.snapshot().verdict.as_ref() else {
        return if claimed_protocol {
            Err(stale_verdict_attention(
                "verdict Attention has no current Delivery verdict",
            ))
        } else {
            Ok(None)
        };
    };
    let Some(stage_run_id) = item.stage_run_id.as_ref() else {
        return if claimed_protocol {
            Err(stale_verdict_attention(
                "verdict Attention has no verification StageRun",
            ))
        } else {
            Ok(None)
        };
    };
    let expected = derive_verdict_attention(
        delivery.snapshot(),
        verdict,
        stage_run_id,
        item.created_at_millis,
    )?;
    let identity_matches = expected.iter().any(|expected| expected.id == item.id);
    if !claimed_protocol && !identity_matches {
        return Ok(None);
    }
    if item.status != AttentionItemStatus::Open {
        return Err(stale_verdict_attention(
            "only an open current verdict Attention item can be resolved",
        ));
    }

    let mut current = Vec::new();
    for stored in &delivery.snapshot().attention_items {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&stored.context) else {
            continue;
        };
        if value.get("protocol").and_then(serde_json::Value::as_str)
            != Some(VERDICT_ATTENTION_PROTOCOL)
        {
            continue;
        }
        let context: VerdictAttentionContext = serde_json::from_value(value)
            .map_err(|_| stale_verdict_attention("verdict Attention context is malformed"))?;
        if context.verdict_id == verdict.id {
            current.push((stored, context));
        }
    }
    if current.len() != expected.len() {
        return Err(stale_verdict_attention(
            "current verdict does not retain its complete computed Attention set",
        ));
    }

    let mut actions = Vec::with_capacity(expected.len());
    for expected_item in &expected {
        let (stored, context) = current
            .iter()
            .find(|(stored, _)| stored.id == expected_item.id)
            .ok_or_else(|| {
                stale_verdict_attention(
                    "verdict Attention is absent from the complete current classification",
                )
            })?;
        let mut normalized = (*stored).clone();
        match normalized.status {
            AttentionItemStatus::Open => {}
            AttentionItemStatus::Resolved => {
                normalized.status = AttentionItemStatus::Open;
                normalized.resolution = None;
                normalized.resolved_by = None;
                normalized.resolved_at_millis = None;
            }
            AttentionItemStatus::Dismissed => {
                return Err(stale_verdict_attention(
                    "computed verdict Attention cannot be dismissed",
                ));
            }
        }
        if normalized != *expected_item
            || context.protocol != VERDICT_ATTENTION_PROTOCOL
            || context.verdict_id != verdict.id
            || context.candidate_ref != verdict.candidate_ref
            || context.stage_run_id != *stage_run_id
        {
            return Err(stale_verdict_attention(
                "verdict Attention differs from its current computed classification",
            ));
        }
        actions.push(context.action.transition_action());
    }
    Ok(Some(actions))
}

fn stale_verdict_attention(message: &'static str) -> CoordinationError {
    CoordinationError::new(CoordinationErrorCode::StaleAttention, message)
}

fn event_from_transition(
    delivery: &Delivery,
    evidence: Vec<EvidenceRef>,
    verdict: DeliveryVerdict,
    attention_items: Vec<AttentionItem>,
) -> DeliveryVerdictSubmittedEvent {
    DeliveryVerdictSubmittedEvent {
        schema_version: 1,
        delivery_id: delivery.id().clone(),
        delivery_revision: delivery.revision(),
        candidate_ref: verdict.candidate_ref.clone(),
        evidence,
        verdict,
        attention_items,
        task_statuses: delivery
            .snapshot()
            .tasks
            .iter()
            .map(|task| DeliveryTaskStatusFact {
                delivery_task_id: task.id.clone(),
                status: task.status,
            })
            .collect(),
        status: delivery.snapshot().status,
        produced_at_millis: delivery.snapshot().updated_at_millis,
    }
}

fn validate_transition_delta(
    source: &Delivery,
    transition: &ComputedVerdictTransition,
) -> Result<(), CoordinationError> {
    let before = source.snapshot();
    let after = transition.delivery.snapshot();
    if after.schema_version != before.schema_version
        || after.id != before.id
        || after.revision != before.revision.saturating_add(1)
        || after.spec != before.spec
        || after.stage_runs != before.stage_runs
        || after.session_bindings != before.session_bindings
        || after.created_at_millis != before.created_at_millis
        || !after.attention_items.starts_with(&before.attention_items)
        || !after.evidence.starts_with(&before.evidence)
        || after.updated_at_millis != transition.event.produced_at_millis
        || after.verdict.as_ref() != Some(&transition.event.verdict)
        || after.status != transition.event.status
        || transition.event.delivery_id != after.id
        || transition.event.delivery_revision != after.revision
        || transition.event.candidate_ref != transition.candidate_ref
        || transition.event.candidate_ref != transition.event.verdict.candidate_ref
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "computed verdict transition changed a fact outside its exact operation delta",
        ));
    }
    let appended_evidence = &after.evidence[before.evidence.len()..];
    let expected_appended = transition
        .event
        .evidence
        .iter()
        .filter(|evidence| {
            !before
                .evidence
                .iter()
                .any(|stored| stored.id == evidence.id)
        })
        .cloned()
        .collect::<Vec<_>>();
    let appended_attention = &after.attention_items[before.attention_items.len()..];
    let task_statuses = after
        .tasks
        .iter()
        .map(|task| DeliveryTaskStatusFact {
            delivery_task_id: task.id.clone(),
            status: task.status,
        })
        .collect::<Vec<_>>();
    if appended_evidence != expected_appended
        || appended_attention != transition.event.attention_items
        || task_statuses != transition.event.task_statuses
        || after.tasks.len() != before.tasks.len()
        || after.tasks.iter().zip(&before.tasks).any(|(next, prior)| {
            next.schema_version != prior.schema_version
                || next.id != prior.id
                || next.delivery_id != prior.delivery_id
                || next.title != prior.title
                || next.goal != prior.goal
                || next.acceptance_criterion_ids != prior.acceptance_criterion_ids
                || next.blocked_by_task_ids != prior.blocked_by_task_ids
                || next.owner != prior.owner
        })
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "computed verdict transition does not contain its exact Evidence, Attention, or task delta",
        ));
    }
    Ok(())
}

fn delivery_digest(delivery: &Delivery) -> Result<String, CoordinationError> {
    let bytes = delivery.encode_json().map_err(|error| {
        CoordinationError::new(
            CoordinationErrorCode::Conflict,
            format!("Delivery source cannot be encoded: {error}"),
        )
    })?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

/// Narrow fixtures for cross-crate transaction tests. This module is absent
/// from normal builds and still constructs every sealed fact through the
/// production candidate, verification, Evidence, and verdict seams.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use winwincode_domain::{
        CodexThreadId, DeliveryId, ExecutionJobId, ProductSessionId, StageRunId, WorkerSessionId,
    };

    use crate::domain::{
        Delivery, DeliveryStage, DeliveryStatus, DeliveryTaskStatus, EvidenceRefType,
        FrozenDeliveryCandidate, SessionBinding, SessionBindingId, StageRun, StageRunActorType,
        StageRunStatus,
        candidate::test_support::frozen_candidate,
        evidence::{
            ResolvedDeliveryEvidence, VerifiedEvidenceOutcome, test_support::resolved_role_evidence,
        },
        test_fixture,
        verification::{
            IndependentVerification, VerificationRole,
            test_support::{
                VerificationFixtureState, fixture_evidence_id, independent_verification,
            },
        },
    };

    #[derive(Debug)]
    pub struct VerdictFixture {
        pub delivery: Delivery,
        pub candidate: FrozenDeliveryCandidate,
        pub verification: IndependentVerification,
        pub evidence: Vec<ResolvedDeliveryEvidence>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum VerdictFixtureOutcome {
        Pass,
        Fail,
    }

    #[must_use]
    pub fn verdict_fixture(
        delivery_id: &DeliveryId,
        outcome: VerdictFixtureOutcome,
    ) -> VerdictFixture {
        let delivery = verifying_delivery(delivery_id);
        let candidate = frozen_candidate(
            &delivery,
            &StageRunId("stage-executor-1".into()),
            &SessionBindingId("binding-executor-1".into()),
        );
        let state = match outcome {
            VerdictFixtureOutcome::Pass => VerificationFixtureState::SettledPass,
            VerdictFixtureOutcome::Fail => VerificationFixtureState::SettledFail,
        };
        let verification = independent_verification(&delivery, &candidate, state, state);
        let criterion_id = &delivery.snapshot().spec.acceptance_criteria[0].id;
        let evidence = [VerificationRole::Reviewer, VerificationRole::Verifier]
            .into_iter()
            .map(|role| {
                resolved_role_evidence(
                    &delivery,
                    &candidate,
                    match role {
                        VerificationRole::Reviewer => "reviewer",
                        VerificationRole::Verifier => "verifier",
                        VerificationRole::AdversarialVerifier => unreachable!(),
                    },
                    EvidenceRefType::Test,
                    match outcome {
                        VerdictFixtureOutcome::Pass => VerifiedEvidenceOutcome::Succeeded,
                        VerdictFixtureOutcome::Fail => VerifiedEvidenceOutcome::Failed,
                    },
                    fixture_evidence_id(role, criterion_id),
                )
            })
            .collect();
        VerdictFixture {
            delivery,
            candidate,
            verification,
            evidence,
        }
    }

    fn verifying_delivery(delivery_id: &DeliveryId) -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.id = delivery_id.clone();
        snapshot.spec.delivery_id = delivery_id.clone();
        snapshot.revision = 1;
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.attention_items.clear();
        for task in &mut snapshot.tasks {
            task.delivery_id = delivery_id.clone();
            task.status = DeliveryTaskStatus::Verifying;
        }

        let task_id = {
            let writer = &mut snapshot.stage_runs[0];
            writer.id = StageRunId("stage-executor-1".into());
            writer.delivery_id = delivery_id.clone();
            writer.stage = DeliveryStage::Executing;
            writer.role = "executor".into();
            writer.status = StageRunStatus::Succeeded;
            writer.started_at_millis = 1_800_000_000_010;
            writer.finished_at_millis = Some(1_800_000_000_020);
            writer.delivery_task_id.clone()
        };
        let writer_binding = &mut snapshot.session_bindings[0];
        writer_binding.id = SessionBindingId("binding-executor-1".into());
        writer_binding.delivery_id = delivery_id.clone();
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
                schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
                id: stage_run_id.clone(),
                delivery_id: delivery_id.clone(),
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
                schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
                id: SessionBindingId(format!("binding-{role}-1")),
                delivery_id: delivery_id.clone(),
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
        Delivery::try_from_snapshot(snapshot).expect("verdict transaction fixture")
    }
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        CodexThreadId, ExecutionJobId, ProductSessionId, StageRunId, WorkerSessionId,
    };

    use super::{SubmitVerdictFacts, compute_verdict_transition, test_support};
    use crate::application::attention::{
        AttentionDecision, ResolveAttentionInput, resolve_attention,
    };
    use crate::domain::{
        AttentionItemStatus, Delivery, DeliveryStage, DeliveryStatus, DeliveryTaskStatus,
        EvidenceRefType, FrozenDeliveryCandidate, SessionBinding, SessionBindingId, StageRun,
        StageRunActorType, StageRunStatus,
        candidate::test_support::frozen_candidate,
        evidence::{VerifiedEvidenceOutcome, test_support::resolved_role_evidence},
        test_fixture,
        verification::{
            VerificationRole,
            test_support::{
                VerificationFixtureState, fixture_evidence_id, independent_verification,
            },
        },
    };

    const PRODUCED_AT_MILLIS: u64 = 1_800_000_000_100;

    #[test]
    fn computed_failure_attention_resolves_to_bounded_rework() {
        let fixture = test_support::verdict_fixture(
            &winwincode_domain::DeliveryId("delivery-verdict-attention-resolution".into()),
            test_support::VerdictFixtureOutcome::Fail,
        );
        let transition = compute_verdict_transition(
            &fixture.delivery,
            SubmitVerdictFacts {
                expected_revision: fixture.delivery.revision(),
                candidate: &fixture.candidate,
                verification: &fixture.verification,
                evidence: &fixture.evidence,
                produced_at_millis: PRODUCED_AT_MILLIS,
            },
        )
        .expect("computed failing verdict");
        let failed = transition.delivery();
        let item = failed
            .snapshot()
            .attention_items
            .iter()
            .find(|item| {
                item.options
                    .iter()
                    .any(|option| option.id == "start-rework")
            })
            .expect("computed start-rework Attention");
        let stage_run_id = item.stage_run_id.clone().expect("verification StageRun");

        let resolved = resolve_attention(
            failed,
            ResolveAttentionInput {
                expected_revision: failed.revision(),
                attention_item_id: item.id.clone(),
                stage_run_id,
                expected_context: item.context.clone(),
                actor: "delivery-reviewer".into(),
                decision: AttentionDecision::Resolved,
                resolution: "Approve the exact bounded remediation scope.".into(),
                now_millis: PRODUCED_AT_MILLIS + 1,
            },
        )
        .expect("current computed failure Attention resolves");

        assert_eq!(resolved.snapshot().status, DeliveryStatus::Reworking);
        assert_eq!(
            resolved
                .snapshot()
                .attention_items
                .iter()
                .find(|stored| stored.id == item.id)
                .expect("resolved Attention")
                .status,
            AttentionItemStatus::Resolved
        );
        assert_eq!(resolved.snapshot().verdict, failed.snapshot().verdict);
        assert_eq!(resolved.snapshot().evidence, failed.snapshot().evidence);
    }

    fn verifying_delivery() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.attention_items.clear();
        snapshot.tasks[0].status = DeliveryTaskStatus::Verifying;

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
                schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
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
                schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
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
        Delivery::try_from_snapshot(snapshot).expect("verifying Delivery")
    }

    fn candidate(delivery: &Delivery) -> FrozenDeliveryCandidate {
        frozen_candidate(
            delivery,
            &StageRunId("stage-executor-1".into()),
            &SessionBindingId("binding-executor-1".into()),
        )
    }

    #[test]
    fn non_passing_verdict_commits_complete_blocking_attention_as_one_transition() {
        let delivery = verifying_delivery();
        let candidate = candidate(&delivery);
        let verification = independent_verification(
            &delivery,
            &candidate,
            VerificationFixtureState::SettledFail,
            VerificationFixtureState::SettledFail,
        );
        let criterion_id = &delivery.snapshot().spec.acceptance_criteria[0].id;
        let evidence = [VerificationRole::Reviewer, VerificationRole::Verifier]
            .into_iter()
            .map(|role| {
                resolved_role_evidence(
                    &delivery,
                    &candidate,
                    match role {
                        VerificationRole::Reviewer => "reviewer",
                        VerificationRole::Verifier => "verifier",
                        VerificationRole::AdversarialVerifier => unreachable!(),
                    },
                    EvidenceRefType::Test,
                    VerifiedEvidenceOutcome::Failed,
                    fixture_evidence_id(role, criterion_id),
                )
            })
            .collect::<Vec<_>>();

        let transition = compute_verdict_transition(
            &delivery,
            SubmitVerdictFacts {
                expected_revision: delivery.revision(),
                candidate: &candidate,
                verification: &verification,
                evidence: &evidence,
                produced_at_millis: PRODUCED_AT_MILLIS,
            },
        )
        .expect("computed fail-closed transition");
        let next = transition.delivery().snapshot();

        assert_eq!(next.revision, delivery.revision() + 1);
        assert_eq!(next.status, DeliveryStatus::NeedsAttention);
        assert!(next.verdict.is_some());
        assert_eq!(next.evidence.len(), 2);
        assert_eq!(next.attention_items.len(), 1);
        assert!(next.attention_items[0].blocking);
        assert_eq!(next.tasks[0].status, DeliveryTaskStatus::Failed);
    }
}
