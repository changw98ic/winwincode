// SPDX-License-Identifier: Apache-2.0

//! Deterministic classification of Delivery failures into one business action.
//!
//! This module validates references to the current Delivery contract, frozen
//! candidate, Evidence, Verdict, Attention, and source `StageRun`. It returns a
//! bounded [`FailurePacket`] plus the next business action. It deliberately
//! does not execute retries, mutate a `Delivery`, or copy Codex/Agent state.

use std::{collections::HashSet, error::Error, fmt};

use serde::Serialize;
use sha2::{Digest, Sha256};
use winwincode_domain::{AttentionItemId, DeliveryId, EvidenceId, StageRunId};

use crate::domain::{
    AcceptanceCriterionId, AttentionItemStatus, Delivery, DeliverySpecId, DeliveryStage,
    DeliveryVerdictId, FrozenDeliveryCandidate, MAX_REFERENCE_LENGTH, MAX_TEXT_LENGTH,
    StageRunActorType, assert_frozen_candidate_current, bounded_text, safe_non_negative,
};

/// The subsystem whose accepted fact reported the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureSourceKind {
    Verifier,
    Observer,
    Provider,
    Runtime,
}

/// Failure facts accepted from an independent verifier or reviewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifierFailure {
    CandidateCounterexample,
    PlanAssumptionInvalidated,
    RequirementAmbiguous,
    AcceptanceContractConflict,
    VerificationInfrastructureUnavailable,
    ModelJudgmentInconclusive,
    ConflictingIndependentFindings,
    EvidenceIntegrityViolation,
}

/// Failure facts accepted from the Delivery drift/policy observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverFailure {
    CandidateDrift,
    PlanDrift,
    RequirementDrift,
    AcceptanceContractDrift,
    InfrastructureDegraded,
    ModelCapabilityGap,
    PolicyDecisionRequired,
    UnsafeState,
}

/// Failure facts accepted from the provider gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailure {
    TransientUnavailable,
    ModelCapabilityGap,
    ManualConfigurationRequired,
    PermanentProtocolViolation,
}

/// Failure facts accepted from the fenced execution runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeFailure {
    CandidateCommandFailure,
    PlanInvariantFailed,
    RequiredInputMissing,
    AcceptanceHarnessMismatch,
    LeaseOrResourceUnavailable,
    ModelContextExhausted,
    PolicyDecisionRequired,
    CancelledOrIntegrityLost,
}

/// A source-specific fact. Callers cannot pair a provider fact with a verifier
/// source kind because the kind is derived from this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "sourceKind", content = "failure", rename_all = "snake_case")]
pub enum FailureSignal {
    Verifier(VerifierFailure),
    Observer(ObserverFailure),
    Provider(ProviderFailure),
    Runtime(RuntimeFailure),
}

impl FailureSignal {
    #[must_use]
    pub const fn source_kind(self) -> FailureSourceKind {
        match self {
            Self::Verifier(_) => FailureSourceKind::Verifier,
            Self::Observer(_) => FailureSourceKind::Observer,
            Self::Provider(_) => FailureSourceKind::Provider,
            Self::Runtime(_) => FailureSourceKind::Runtime,
        }
    }

    const fn route(self) -> FailureRoute {
        match self {
            Self::Verifier(failure) => match failure {
                VerifierFailure::CandidateCounterexample => FailureRoute::Repair,
                VerifierFailure::PlanAssumptionInvalidated => FailureRoute::Replan,
                VerifierFailure::RequirementAmbiguous => FailureRoute::Clarification,
                VerifierFailure::AcceptanceContractConflict => FailureRoute::AcceptanceReview,
                VerifierFailure::VerificationInfrastructureUnavailable => FailureRoute::InfraRetry,
                VerifierFailure::ModelJudgmentInconclusive => FailureRoute::ModelEscalation,
                VerifierFailure::ConflictingIndependentFindings => FailureRoute::HumanReview,
                VerifierFailure::EvidenceIntegrityViolation => FailureRoute::Abort,
            },
            Self::Observer(failure) => match failure {
                ObserverFailure::CandidateDrift => FailureRoute::Repair,
                ObserverFailure::PlanDrift => FailureRoute::Replan,
                ObserverFailure::RequirementDrift => FailureRoute::Clarification,
                ObserverFailure::AcceptanceContractDrift => FailureRoute::AcceptanceReview,
                ObserverFailure::InfrastructureDegraded => FailureRoute::InfraRetry,
                ObserverFailure::ModelCapabilityGap => FailureRoute::ModelEscalation,
                ObserverFailure::PolicyDecisionRequired => FailureRoute::HumanReview,
                ObserverFailure::UnsafeState => FailureRoute::Abort,
            },
            Self::Provider(failure) => match failure {
                ProviderFailure::TransientUnavailable => FailureRoute::InfraRetry,
                ProviderFailure::ModelCapabilityGap => FailureRoute::ModelEscalation,
                ProviderFailure::ManualConfigurationRequired => FailureRoute::HumanReview,
                ProviderFailure::PermanentProtocolViolation => FailureRoute::Abort,
            },
            Self::Runtime(failure) => match failure {
                RuntimeFailure::CandidateCommandFailure => FailureRoute::Repair,
                RuntimeFailure::PlanInvariantFailed => FailureRoute::Replan,
                RuntimeFailure::RequiredInputMissing => FailureRoute::Clarification,
                RuntimeFailure::AcceptanceHarnessMismatch => FailureRoute::AcceptanceReview,
                RuntimeFailure::LeaseOrResourceUnavailable => FailureRoute::InfraRetry,
                RuntimeFailure::ModelContextExhausted => FailureRoute::ModelEscalation,
                RuntimeFailure::PolicyDecisionRequired => FailureRoute::HumanReview,
                RuntimeFailure::CancelledOrIntegrityLost => FailureRoute::Abort,
            },
        }
    }
}

/// The only business actions the router may request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureRoute {
    Repair,
    Replan,
    Clarification,
    AcceptanceReview,
    InfraRetry,
    ModelEscalation,
    HumanReview,
    Abort,
}

/// Exact contract identity retained in every failure packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureContractRef {
    delivery_id: DeliveryId,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    affected_criterion_ids: Vec<AcceptanceCriterionId>,
}

impl FailureContractRef {
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
    pub fn affected_criterion_ids(&self) -> &[AcceptanceCriterionId] {
        &self.affected_criterion_ids
    }
}

/// A bounded, reproducible statement of how observed behavior differs from
/// the current contract or execution invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureCounterexample {
    pub expected: String,
    pub observed: String,
    pub reproduction_ref: String,
}

/// Accepted source identity retained by the packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureSource {
    signal: FailureSignal,
    source_ref: String,
    stage_run_id: Option<StageRunId>,
}

impl FailureSource {
    #[must_use]
    pub const fn kind(&self) -> FailureSourceKind {
        self.signal.source_kind()
    }

    #[must_use]
    pub const fn signal(&self) -> FailureSignal {
        self.signal
    }

    #[must_use]
    pub fn source_ref(&self) -> &str {
        &self.source_ref
    }

    #[must_use]
    pub fn stage_run_id(&self) -> Option<&StageRunId> {
        self.stage_run_id.as_ref()
    }
}

/// An immutable packet for the next application service. It contains only
/// canonical references and bounded observations, not Agent or retry state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailurePacket {
    schema_version: u8,
    id: String,
    route: FailureRoute,
    contract: FailureContractRef,
    candidate_ref: String,
    counterexample: FailureCounterexample,
    source: FailureSource,
    evidence_ref_ids: Vec<EvidenceId>,
    verdict_id: Option<DeliveryVerdictId>,
    attention_item_id: Option<AttentionItemId>,
    occurred_at_millis: u64,
}

impl FailurePacket {
    #[must_use]
    pub const fn schema_version(&self) -> u8 {
        self.schema_version
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn route(&self) -> FailureRoute {
        self.route
    }

    #[must_use]
    pub fn contract(&self) -> &FailureContractRef {
        &self.contract
    }

    #[must_use]
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    #[must_use]
    pub fn counterexample(&self) -> &FailureCounterexample {
        &self.counterexample
    }

    #[must_use]
    pub fn source(&self) -> &FailureSource {
        &self.source
    }

    #[must_use]
    pub fn evidence_ref_ids(&self) -> &[EvidenceId] {
        &self.evidence_ref_ids
    }

    #[must_use]
    pub fn verdict_id(&self) -> Option<&DeliveryVerdictId> {
        self.verdict_id.as_ref()
    }

    #[must_use]
    pub fn attention_item_id(&self) -> Option<&AttentionItemId> {
        self.attention_item_id.as_ref()
    }

    #[must_use]
    pub const fn occurred_at_millis(&self) -> u64 {
        self.occurred_at_millis
    }
}

/// The router's complete output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureRoutingDecision {
    next_action: FailureRoute,
    packet: FailurePacket,
}

impl FailureRoutingDecision {
    #[must_use]
    pub const fn next_action(&self) -> FailureRoute {
        self.next_action
    }

    #[must_use]
    pub fn packet(&self) -> &FailurePacket {
        &self.packet
    }
}

/// Observable and canonical references accepted by [`route_failure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteFailureInput {
    pub signal: FailureSignal,
    pub source_ref: String,
    pub stage_run_id: Option<StageRunId>,
    pub counterexample: FailureCounterexample,
    pub affected_criterion_ids: Vec<AcceptanceCriterionId>,
    pub evidence_ref_ids: Vec<EvidenceId>,
    pub verdict_id: Option<DeliveryVerdictId>,
    pub attention_item_id: Option<AttentionItemId>,
    pub occurred_at_millis: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureRoutingErrorCode {
    InvalidInput,
    StaleCandidate,
    InvalidSource,
    ReferenceMismatch,
    InvalidTime,
    EncodingFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureRoutingError {
    code: FailureRoutingErrorCode,
    message: String,
}

impl FailureRoutingError {
    #[must_use]
    pub const fn code(&self) -> FailureRoutingErrorCode {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for FailureRoutingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for FailureRoutingError {}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FailurePacketIdentity<'packet> {
    schema_version: u8,
    route: FailureRoute,
    contract: &'packet FailureContractRef,
    candidate_ref: &'packet str,
    counterexample: &'packet FailureCounterexample,
    source: &'packet FailureSource,
    evidence_ref_ids: &'packet [EvidenceId],
    verdict_id: Option<&'packet DeliveryVerdictId>,
    attention_item_id: Option<&'packet AttentionItemId>,
    occurred_at_millis: u64,
}

/// Classifies one accepted failure and returns its next business action.
///
/// The function does not mutate Delivery state or run the requested action.
/// Only an explicitly transient infrastructure fact can request
/// [`FailureRoute::InfraRetry`]; candidate/code failures request repair.
///
/// # Errors
///
/// Rejects a stale candidate, malformed counterexample/source, foreign or
/// stale canonical references, a source that does not match its subsystem, or
/// a report time that precedes the current Delivery/source run.
pub fn route_failure(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    mut input: RouteFailureInput,
) -> Result<FailureRoutingDecision, FailureRoutingError> {
    assert_frozen_candidate_current(delivery, candidate).map_err(|error| {
        routing_error(
            FailureRoutingErrorCode::StaleCandidate,
            format!("failure candidate is not current: {error}"),
        )
    })?;

    let route = input.signal.route();
    validate_time(delivery, input.occurred_at_millis)?;
    validate_counterexample(&input.counterexample)?;
    validate_source(delivery, &input)?;
    normalize_and_validate_criteria(delivery, route, &mut input.affected_criterion_ids)?;
    normalize_and_validate_evidence(delivery, candidate, &mut input.evidence_ref_ids)?;
    validate_verdict(delivery, candidate, input.verdict_id.as_ref())?;
    validate_attention(delivery, input.attention_item_id.as_ref())?;

    let contract = FailureContractRef {
        delivery_id: delivery.id().clone(),
        delivery_spec_id: delivery.snapshot().spec.id.clone(),
        delivery_spec_revision: delivery.snapshot().spec.revision,
        affected_criterion_ids: input.affected_criterion_ids,
    };
    let source = FailureSource {
        signal: input.signal,
        source_ref: input.source_ref,
        stage_run_id: input.stage_run_id,
    };
    let identity = FailurePacketIdentity {
        schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
        route,
        contract: &contract,
        candidate_ref: candidate.candidate_ref(),
        counterexample: &input.counterexample,
        source: &source,
        evidence_ref_ids: &input.evidence_ref_ids,
        verdict_id: input.verdict_id.as_ref(),
        attention_item_id: input.attention_item_id.as_ref(),
        occurred_at_millis: input.occurred_at_millis,
    };
    let encoded = serde_json::to_vec(&identity).map_err(|error| {
        routing_error(
            FailureRoutingErrorCode::EncodingFailure,
            format!("failure packet identity cannot be encoded: {error}"),
        )
    })?;
    let id = format!("failure:sha256:{:x}", Sha256::digest(encoded));
    let packet = FailurePacket {
        schema_version: crate::domain::DELIVERY_SCHEMA_VERSION,
        id,
        route,
        contract,
        candidate_ref: candidate.candidate_ref().into(),
        counterexample: input.counterexample,
        source,
        evidence_ref_ids: input.evidence_ref_ids,
        verdict_id: input.verdict_id,
        attention_item_id: input.attention_item_id,
        occurred_at_millis: input.occurred_at_millis,
    };
    Ok(FailureRoutingDecision {
        next_action: route,
        packet,
    })
}

fn validate_time(delivery: &Delivery, occurred_at_millis: u64) -> Result<(), FailureRoutingError> {
    safe_non_negative(occurred_at_millis, "failure.occurredAtMillis")
        .map_err(|error| routing_error(FailureRoutingErrorCode::InvalidTime, error.to_string()))?;
    if occurred_at_millis < delivery.snapshot().updated_at_millis {
        return Err(routing_error(
            FailureRoutingErrorCode::InvalidTime,
            "failure report precedes the current Delivery state",
        ));
    }
    Ok(())
}

fn validate_counterexample(
    counterexample: &FailureCounterexample,
) -> Result<(), FailureRoutingError> {
    bounded_text(
        &counterexample.expected,
        "failure.counterexample.expected",
        MAX_TEXT_LENGTH,
    )
    .and_then(|()| {
        bounded_text(
            &counterexample.observed,
            "failure.counterexample.observed",
            MAX_TEXT_LENGTH,
        )
    })
    .and_then(|()| {
        bounded_text(
            &counterexample.reproduction_ref,
            "failure.counterexample.reproductionRef",
            MAX_REFERENCE_LENGTH,
        )
    })
    .map_err(|error| routing_error(FailureRoutingErrorCode::InvalidInput, error.to_string()))
}

fn validate_source(
    delivery: &Delivery,
    input: &RouteFailureInput,
) -> Result<(), FailureRoutingError> {
    bounded_text(
        &input.source_ref,
        "failure.source.sourceRef",
        MAX_REFERENCE_LENGTH,
    )
    .map_err(|error| routing_error(FailureRoutingErrorCode::InvalidSource, error.to_string()))?;

    let source_kind = input.signal.source_kind();
    let Some(stage_run_id) = &input.stage_run_id else {
        return if source_kind == FailureSourceKind::Observer {
            Ok(())
        } else {
            Err(routing_error(
                FailureRoutingErrorCode::InvalidSource,
                "Verifier, Provider, and Runtime failures require an exact StageRun",
            ))
        };
    };
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == *stage_run_id)
        .ok_or_else(|| {
            routing_error(
                FailureRoutingErrorCode::InvalidSource,
                "failure source StageRun is not part of the current Delivery",
            )
        })?;
    if input.occurred_at_millis < run.started_at_millis {
        return Err(routing_error(
            FailureRoutingErrorCode::InvalidTime,
            "failure report precedes its source StageRun",
        ));
    }
    let source_matches = match source_kind {
        FailureSourceKind::Verifier => {
            run.stage == DeliveryStage::Verifying
                && run.actor_type == StageRunActorType::Codex
                && matches!(
                    run.role.as_str(),
                    "verifier" | "reviewer" | "adversarial-verifier"
                )
        }
        FailureSourceKind::Observer => true,
        FailureSourceKind::Provider | FailureSourceKind::Runtime => {
            run.actor_type == StageRunActorType::Codex
        }
    };
    if !source_matches {
        return Err(routing_error(
            FailureRoutingErrorCode::InvalidSource,
            "failure signal does not match the source StageRun subsystem",
        ));
    }
    Ok(())
}

fn normalize_and_validate_criteria(
    delivery: &Delivery,
    route: FailureRoute,
    criterion_ids: &mut [AcceptanceCriterionId],
) -> Result<(), FailureRoutingError> {
    let mut unique = HashSet::with_capacity(criterion_ids.len());
    for criterion_id in criterion_ids.iter() {
        if !unique.insert(criterion_id.0.as_str()) {
            return Err(routing_error(
                FailureRoutingErrorCode::InvalidInput,
                "failure contains duplicate acceptance criterion references",
            ));
        }
        if !delivery
            .snapshot()
            .spec
            .acceptance_criteria
            .iter()
            .any(|criterion| criterion.id == *criterion_id)
        {
            return Err(routing_error(
                FailureRoutingErrorCode::ReferenceMismatch,
                "failure references an acceptance criterion outside the current DeliverySpec",
            ));
        }
    }
    if criterion_ids.is_empty()
        && matches!(
            route,
            FailureRoute::Repair | FailureRoute::Clarification | FailureRoute::AcceptanceReview
        )
    {
        return Err(routing_error(
            FailureRoutingErrorCode::InvalidInput,
            "repair, clarification, and acceptance review require an affected criterion",
        ));
    }
    criterion_ids.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(())
}

fn normalize_and_validate_evidence(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    evidence_ids: &mut [EvidenceId],
) -> Result<(), FailureRoutingError> {
    let mut unique = HashSet::with_capacity(evidence_ids.len());
    for evidence_id in evidence_ids.iter() {
        if !unique.insert(evidence_id.0.as_str()) {
            return Err(routing_error(
                FailureRoutingErrorCode::InvalidInput,
                "failure contains duplicate Evidence references",
            ));
        }
        let evidence = delivery
            .snapshot()
            .evidence
            .iter()
            .find(|evidence| evidence.id == *evidence_id)
            .ok_or_else(|| {
                routing_error(
                    FailureRoutingErrorCode::ReferenceMismatch,
                    "failure Evidence is not part of the current Delivery",
                )
            })?;
        if evidence.delivery_spec_id != delivery.snapshot().spec.id
            || evidence.delivery_spec_revision != delivery.snapshot().spec.revision
            || evidence.candidate_ref != candidate.candidate_ref()
        {
            return Err(routing_error(
                FailureRoutingErrorCode::ReferenceMismatch,
                "failure Evidence belongs to another contract or candidate",
            ));
        }
    }
    evidence_ids.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(())
}

fn validate_verdict(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    verdict_id: Option<&DeliveryVerdictId>,
) -> Result<(), FailureRoutingError> {
    let Some(verdict_id) = verdict_id else {
        return Ok(());
    };
    let current = delivery.snapshot().verdict.as_ref().is_some_and(|verdict| {
        verdict.id == *verdict_id
            && verdict.delivery_spec_id == delivery.snapshot().spec.id
            && verdict.candidate_ref == candidate.candidate_ref()
    });
    if current {
        Ok(())
    } else {
        Err(routing_error(
            FailureRoutingErrorCode::ReferenceMismatch,
            "failure Verdict is not current for this contract and candidate",
        ))
    }
}

fn validate_attention(
    delivery: &Delivery,
    attention_item_id: Option<&AttentionItemId>,
) -> Result<(), FailureRoutingError> {
    let Some(attention_item_id) = attention_item_id else {
        return Ok(());
    };
    let current = delivery.snapshot().attention_items.iter().any(|item| {
        item.id == *attention_item_id
            && item.delivery_spec_id == delivery.snapshot().spec.id
            && item.status == AttentionItemStatus::Open
    });
    if current {
        Ok(())
    } else {
        Err(routing_error(
            FailureRoutingErrorCode::ReferenceMismatch,
            "failure Attention is not open for the current contract",
        ))
    }
}

fn routing_error(code: FailureRoutingErrorCode, message: impl Into<String>) -> FailureRoutingError {
    FailureRoutingError {
        code,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{DeliveryId, EvidenceId, StageRunId};

    use super::*;
    use crate::{
        application::verdict::{
            SubmitVerdictFacts, compute_verdict_transition,
            test_support::{VerdictFixture, VerdictFixtureOutcome, verdict_fixture},
        },
        domain::Delivery,
    };

    fn fixture(outcome: VerdictFixtureOutcome) -> VerdictFixture {
        verdict_fixture(
            &DeliveryId("dlv_01J00000000000000000000465".into()),
            outcome,
        )
    }

    fn route_input(delivery: &Delivery, signal: FailureSignal) -> RouteFailureInput {
        let stage_run_id = match signal.source_kind() {
            FailureSourceKind::Verifier => Some(StageRunId("stage-verifier-1".into())),
            FailureSourceKind::Observer => None,
            FailureSourceKind::Provider | FailureSourceKind::Runtime => {
                Some(StageRunId("stage-executor-1".into()))
            }
        };
        RouteFailureInput {
            signal,
            source_ref: format!("failure-source:{:?}", signal.source_kind()).to_ascii_lowercase(),
            stage_run_id,
            counterexample: FailureCounterexample {
                expected: "The current acceptance criterion is satisfied.".into(),
                observed: "The accepted source fact shows a reproducible mismatch.".into(),
                reproduction_ref: "artifact:failure-counterexample".into(),
            },
            affected_criterion_ids: vec![
                delivery.snapshot().spec.acceptance_criteria[0].id.clone(),
            ],
            evidence_ref_ids: Vec::new(),
            verdict_id: None,
            attention_item_id: None,
            occurred_at_millis: delivery.snapshot().updated_at_millis + 10,
        }
    }

    #[test]
    fn every_failure_state_routes_to_one_explicit_business_action() {
        let cases = [
            (
                RuntimeFailure::CandidateCommandFailure,
                FailureRoute::Repair,
            ),
            (RuntimeFailure::PlanInvariantFailed, FailureRoute::Replan),
            (
                RuntimeFailure::RequiredInputMissing,
                FailureRoute::Clarification,
            ),
            (
                RuntimeFailure::AcceptanceHarnessMismatch,
                FailureRoute::AcceptanceReview,
            ),
            (
                RuntimeFailure::LeaseOrResourceUnavailable,
                FailureRoute::InfraRetry,
            ),
            (
                RuntimeFailure::ModelContextExhausted,
                FailureRoute::ModelEscalation,
            ),
            (
                RuntimeFailure::PolicyDecisionRequired,
                FailureRoute::HumanReview,
            ),
            (
                RuntimeFailure::CancelledOrIntegrityLost,
                FailureRoute::Abort,
            ),
        ];

        for (failure, expected) in cases {
            let fixture = fixture(VerdictFixtureOutcome::Fail);
            let decision = route_failure(
                &fixture.delivery,
                &fixture.candidate,
                route_input(&fixture.delivery, FailureSignal::Runtime(failure)),
            )
            .expect("typed failure route");

            assert_eq!(decision.next_action(), expected);
            assert_eq!(decision.packet().route(), expected);
        }
    }

    #[test]
    fn each_source_has_deterministic_source_specific_classification() {
        let cases = [
            (
                FailureSignal::Verifier(VerifierFailure::CandidateCounterexample),
                FailureRoute::Repair,
            ),
            (
                FailureSignal::Observer(ObserverFailure::AcceptanceContractDrift),
                FailureRoute::AcceptanceReview,
            ),
            (
                FailureSignal::Provider(ProviderFailure::TransientUnavailable),
                FailureRoute::InfraRetry,
            ),
            (
                FailureSignal::Provider(ProviderFailure::ModelCapabilityGap),
                FailureRoute::ModelEscalation,
            ),
        ];

        for (signal, expected) in cases {
            let fixture = fixture(VerdictFixtureOutcome::Fail);
            let decision = route_failure(
                &fixture.delivery,
                &fixture.candidate,
                route_input(&fixture.delivery, signal),
            )
            .expect("source-specific route");
            assert_eq!(decision.next_action(), expected);
            assert_eq!(decision.packet().source().kind(), signal.source_kind());
        }
    }

    #[test]
    fn candidate_code_failure_never_becomes_a_blind_infrastructure_retry() {
        let fixture = fixture(VerdictFixtureOutcome::Fail);
        let decision = route_failure(
            &fixture.delivery,
            &fixture.candidate,
            route_input(
                &fixture.delivery,
                FailureSignal::Runtime(RuntimeFailure::CandidateCommandFailure),
            ),
        )
        .expect("candidate failure route");

        assert_eq!(decision.next_action(), FailureRoute::Repair);
        assert_ne!(decision.next_action(), FailureRoute::InfraRetry);
    }

    #[test]
    fn same_failure_and_reordered_contract_refs_produce_the_same_packet() {
        let fixture = fixture(VerdictFixtureOutcome::Fail);
        let signal = FailureSignal::Verifier(VerifierFailure::CandidateCounterexample);
        let mut first = route_input(&fixture.delivery, signal);
        first.affected_criterion_ids = fixture
            .delivery
            .snapshot()
            .spec
            .acceptance_criteria
            .iter()
            .map(|criterion| criterion.id.clone())
            .collect();
        let mut second = first.clone();
        second.affected_criterion_ids.reverse();

        let first = route_failure(&fixture.delivery, &fixture.candidate, first)
            .expect("first deterministic route");
        let second = route_failure(&fixture.delivery, &fixture.candidate, second)
            .expect("second deterministic route");

        assert_eq!(first, second);
        assert!(first.packet().id().starts_with("failure:sha256:"));
    }

    #[test]
    fn packet_retains_current_contract_candidate_counterexample_and_canonical_refs() {
        let fixture = fixture(VerdictFixtureOutcome::Fail);
        let transition = compute_verdict_transition(
            &fixture.delivery,
            SubmitVerdictFacts {
                expected_revision: fixture.delivery.revision(),
                candidate: &fixture.candidate,
                verification: &fixture.verification,
                evidence: &fixture.evidence,
                produced_at_millis: fixture.delivery.snapshot().updated_at_millis + 10,
            },
        )
        .expect("failing verdict transition");
        let delivery = transition.delivery();
        let verdict = delivery
            .snapshot()
            .verdict
            .as_ref()
            .expect("current verdict");
        let attention = delivery
            .snapshot()
            .attention_items
            .first()
            .expect("current failure Attention");
        let mut input = route_input(
            delivery,
            FailureSignal::Verifier(VerifierFailure::CandidateCounterexample),
        );
        input.evidence_ref_ids = delivery
            .snapshot()
            .evidence
            .iter()
            .map(|evidence| evidence.id.clone())
            .collect();
        input.verdict_id = Some(verdict.id.clone());
        input.attention_item_id = Some(attention.id.clone());

        let decision = route_failure(delivery, &fixture.candidate, input)
            .expect("failure with canonical references");
        let packet = decision.packet();

        assert_eq!(
            packet.schema_version(),
            crate::domain::DELIVERY_SCHEMA_VERSION
        );
        assert_eq!(packet.contract().delivery_id(), delivery.id());
        assert_eq!(
            packet.contract().delivery_spec_id(),
            &delivery.snapshot().spec.id
        );
        assert_eq!(
            packet.contract().delivery_spec_revision(),
            delivery.snapshot().spec.revision
        );
        assert_eq!(packet.candidate_ref(), fixture.candidate.candidate_ref());
        assert_eq!(packet.verdict_id(), Some(&verdict.id));
        assert_eq!(packet.attention_item_id(), Some(&attention.id));
        assert_eq!(
            packet.evidence_ref_ids().len(),
            delivery.snapshot().evidence.len()
        );
        assert!(!packet.counterexample().observed.is_empty());
        assert!(!packet.source().source_ref().is_empty());
        assert_eq!(
            packet.source().stage_run_id(),
            Some(&StageRunId("stage-verifier-1".into()))
        );
    }

    #[test]
    fn router_rejects_stale_candidate_foreign_refs_and_wrong_source_stage() {
        let fixture = fixture(VerdictFixtureOutcome::Fail);
        let mut stale_snapshot = fixture.delivery.clone().into_snapshot();
        stale_snapshot.spec.base_revision = "changed-base-revision".into();
        let stale_delivery = Delivery::try_from_snapshot(stale_snapshot).expect("changed spec");
        let stale = route_failure(
            &stale_delivery,
            &fixture.candidate,
            route_input(
                &stale_delivery,
                FailureSignal::Runtime(RuntimeFailure::CandidateCommandFailure),
            ),
        )
        .expect_err("stale candidate");
        assert_eq!(stale.code(), FailureRoutingErrorCode::StaleCandidate);

        let mut foreign_evidence = route_input(
            &fixture.delivery,
            FailureSignal::Verifier(VerifierFailure::CandidateCounterexample),
        );
        foreign_evidence.evidence_ref_ids = vec![EvidenceId("evidence-foreign".into())];
        let foreign = route_failure(&fixture.delivery, &fixture.candidate, foreign_evidence)
            .expect_err("foreign Evidence");
        assert_eq!(foreign.code(), FailureRoutingErrorCode::ReferenceMismatch);

        let mut wrong_stage = route_input(
            &fixture.delivery,
            FailureSignal::Verifier(VerifierFailure::CandidateCounterexample),
        );
        wrong_stage.stage_run_id = Some(StageRunId("stage-executor-1".into()));
        let wrong_stage = route_failure(&fixture.delivery, &fixture.candidate, wrong_stage)
            .expect_err("non-verifier source stage");
        assert_eq!(wrong_stage.code(), FailureRoutingErrorCode::InvalidSource);
    }

    #[test]
    fn packet_serialization_keeps_structured_source_and_route() {
        let fixture = fixture(VerdictFixtureOutcome::Fail);
        let decision = route_failure(
            &fixture.delivery,
            &fixture.candidate,
            route_input(
                &fixture.delivery,
                FailureSignal::Provider(ProviderFailure::ModelCapabilityGap),
            ),
        )
        .expect("provider route");
        let value = serde_json::to_value(decision.packet()).expect("packet JSON");

        assert_eq!(value["route"], "model_escalation");
        assert_eq!(value["source"]["signal"]["sourceKind"], "provider");
        assert_eq!(value["source"]["signal"]["failure"], "model_capability_gap");
        assert!(value["contract"]["deliverySpecId"].is_string());
        assert!(value["counterexample"]["reproductionRef"].is_string());
    }
}
