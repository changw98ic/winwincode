// SPDX-License-Identifier: Apache-2.0

//! Production reconstruction of a Delivery verdict from sealed durable facts.

use std::collections::{BTreeMap, HashSet};
use std::{error::Error, fmt};

use serde::Deserialize;
use serde_json::Value;
use winwincode_domain::{ExecutionEventId, ExecutionSequence};
use winwincode_storage::ValidatedGitSourceArtifact;

use crate::application::stage::DeliveryTerminalOutcomeFacts;

use super::{
    FrozenDeliveryCandidate, freeze_delivery_candidate_from_source,
    validated_git_snapshot_from_candidate, validated_git_snapshot_from_source,
};
use crate::domain::evidence::{
    EvidenceRefType, EvidenceSource, ResolveDeliveryEvidenceInput, ResolvedDeliveryEvidence,
    VerifiedEvidenceOutcome, accepted_runtime_source, checkout_attestation_from_snapshot,
    resolve_delivery_evidence,
};
use crate::domain::verification::{
    AcceptedVerificationJobOutcomeFact, IndependentVerification, VerificationFacts,
    VerificationFindingConclusion, VerificationFindingFact, VerificationPermissionProfile,
    VerificationRole, VerificationSessionFacts, VerificationWorkspaceMode,
    validate_independent_verification,
};
use crate::domain::{
    AcceptanceCriterionId, Delivery, DeliverySpecId, DeliveryValidationError, SessionBindingId,
};

const VERIFICATION_RESULT_PROTOCOL: &str = "winwincode.independent-verification-result.v1";
const SESSION_POLICY_PROTOCOL: &str = "winwincode.verification-session-policy.v1";
const JSON_CONTENT_TYPE: &str = "application/json";
const MAX_RUNTIME_EVENTS: usize = 100_000;

/// One event read from the canonical accepted runtime ledger.
///
/// The Control Plane derives `occurred_at_millis` from the event's canonical
/// `Instant`; the verdict resolver never accepts an outcome or identity here.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductionRuntimeEvent {
    category: ProductionRuntimeEventCategory,
    event_id: ExecutionEventId,
    sequence: u64,
    occurred_at_millis: u64,
    payload: Option<ProductionRuntimePayload>,
}

impl ProductionRuntimeEvent {
    /// Seals one event already read from the canonical runtime ledger.
    #[must_use]
    pub const fn from_durable_ledger(
        category: ProductionRuntimeEventCategory,
        event_id: ExecutionEventId,
        sequence: u64,
        occurred_at_millis: u64,
        payload: Option<ProductionRuntimePayload>,
    ) -> Self {
        Self {
            category,
            event_id,
            sequence,
            occurred_at_millis,
            payload,
        }
    }
}

/// Delivery-owned classification of one accepted runtime event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionRuntimeEventCategory {
    Lifecycle,
    Activity,
    Command,
    Test,
    Diff,
    Usage,
    Attention,
    Diagnostic,
}

/// Payload bytes whose base64 form and digest were already checked by the
/// Control Plane's canonical runtime-ledger adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductionRuntimePayload {
    content_type: String,
    bytes: Vec<u8>,
}

impl ProductionRuntimePayload {
    /// Seals payload bytes after the Control Plane has checked the durable
    /// transport encoding and digest.
    #[must_use]
    pub fn from_validated_bytes(content_type: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            content_type: content_type.into(),
            bytes,
        }
    }
}

/// Durable runtime facts for one independent verification role.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductionVerificationRuntime {
    terminal: DeliveryTerminalOutcomeFacts,
    source: ValidatedGitSourceArtifact,
    events: Vec<ProductionRuntimeEvent>,
    read_only_candidate_source: bool,
}

impl ProductionVerificationRuntime {
    /// Joins one settled Worker outcome, its exact Git Artifact, and the
    /// accepted append-only runtime ledger. Role and execution identity remain
    /// derived from those sealed facts.
    #[must_use]
    pub fn from_durable(
        terminal: DeliveryTerminalOutcomeFacts,
        source: ValidatedGitSourceArtifact,
        events: Vec<ProductionRuntimeEvent>,
    ) -> Self {
        Self {
            terminal,
            source,
            events,
            read_only_candidate_source: false,
        }
    }

    /// Joins a verification terminal and runtime ledger to the current
    /// writer's validated candidate source. Read-only verification consumes
    /// that checkout and therefore does not produce a second candidate
    /// Artifact of its own.
    #[must_use]
    pub fn from_durable_read_only(
        terminal: DeliveryTerminalOutcomeFacts,
        candidate_source: ValidatedGitSourceArtifact,
        events: Vec<ProductionRuntimeEvent>,
    ) -> Self {
        Self {
            terminal,
            source: candidate_source,
            events,
            read_only_candidate_source: true,
        }
    }
}

/// Sealed candidate, verification, and Evidence facts for verdict computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductionVerdictFacts {
    candidate: FrozenDeliveryCandidate,
    verification: IndependentVerification,
    evidence: Vec<ResolvedDeliveryEvidence>,
    produced_at_millis: u64,
}

impl ProductionVerdictFacts {
    /// Consumes the sealed result for the Control Plane verdict boundary.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        FrozenDeliveryCandidate,
        IndependentVerification,
        Vec<ResolvedDeliveryEvidence>,
        u64,
    ) {
        (
            self.candidate,
            self.verification,
            self.evidence,
            self.produced_at_millis,
        )
    }
}

/// Failure while reconciling durable production verdict facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductionVerdictResolutionError {
    message: String,
}

impl fmt::Display for ProductionVerdictResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProductionVerdictResolutionError {}

/// Resolves exact candidate, verification, and Evidence facts from production
/// durable records.
///
/// # Errors
///
/// Rejects missing roles, non-contiguous or changed runtime events, an invalid
/// policy attestation, stale candidate/spec/runtime positions, malformed
/// structured findings, and any terminal, Artifact, lease, fence, session, or
/// Git identity drift.
pub fn resolve_production_verdict(
    delivery: &Delivery,
    writer_source: &ValidatedGitSourceArtifact,
    writer_terminal: &DeliveryTerminalOutcomeFacts,
    verification: Vec<ProductionVerificationRuntime>,
) -> Result<ProductionVerdictFacts, ProductionVerdictResolutionError> {
    let candidate = freeze_delivery_candidate_from_source(delivery, writer_source, writer_terminal)
        .map_err(|error_value| resolution_error(&error_value))?;
    let mut sessions = Vec::with_capacity(verification.len());
    let mut evidence = Vec::new();
    let mut roles = HashSet::with_capacity(verification.len());
    let mut produced_at_millis = candidate.producer_finished_at_millis();

    for runtime in verification {
        let resolved = resolve_verification_runtime(delivery, &candidate, &runtime)?;
        if !roles.insert(resolved.role) {
            return Err(error("durable verification repeats one current role"));
        }
        produced_at_millis = produced_at_millis.max(resolved.finished_at_millis);
        sessions.push(resolved.session);
        evidence.extend(resolved.evidence);
    }
    let required_roles = required_roles(delivery)?;
    if roles.len() != required_roles.len()
        || required_roles.iter().any(|role| !roles.contains(role))
    {
        return Err(error(
            "durable verification does not contain every exact current role",
        ));
    }
    sessions.sort_by_key(|session| role_order(session.role));
    evidence.sort_by(|left, right| left.evidence().id.0.cmp(&right.evidence().id.0));
    evidence.dedup_by(|left, right| left.evidence().id == right.evidence().id);
    let verification = validate_independent_verification(
        delivery,
        &candidate,
        &VerificationFacts {
            required_roles,
            sessions,
        },
    )
    .map_err(|error_value| resolution_error(&error_value))?;
    produced_at_millis = produced_at_millis
        .checked_add(2)
        .ok_or_else(|| error("verdict production time exceeds the durable range"))?;

    Ok(ProductionVerdictFacts {
        candidate,
        verification,
        evidence,
        produced_at_millis,
    })
}

struct ResolvedVerificationRuntime {
    role: VerificationRole,
    session: VerificationSessionFacts,
    evidence: Vec<ResolvedDeliveryEvidence>,
    finished_at_millis: u64,
}

fn resolve_verification_runtime(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    runtime: &ProductionVerificationRuntime,
) -> Result<ResolvedVerificationRuntime, ProductionVerdictResolutionError> {
    let (snapshot, verified_terminal) = if runtime.read_only_candidate_source {
        validated_git_snapshot_from_candidate(delivery, candidate, &runtime.terminal)
    } else {
        validated_git_snapshot_from_source(delivery, &runtime.source, &runtime.terminal)
    }
    .map_err(|error_value| resolution_error(&error_value))?;
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == *verified_terminal.stage_run_id())
        .ok_or_else(|| error("verification terminal StageRun is missing"))?;
    let role = verification_role(&run.role)?;
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .ok_or_else(|| error("verification terminal SessionBinding is missing"))?;
    let terminal = if runtime.read_only_candidate_source {
        AcceptedVerificationJobOutcomeFact::from_verified_read_only_outcome(
            &verified_terminal,
            &snapshot,
            run.role.clone(),
        )
    } else {
        AcceptedVerificationJobOutcomeFact::from_verified_outcome(
            &verified_terminal,
            &snapshot,
            run.role.clone(),
        )
    }
    .map_err(|error_value| resolution_error(&error_value))?;
    let events = validated_runtime_events(&runtime.events, &terminal)?;
    validate_policy_attestation(&events, candidate)?;
    let result = structured_result(&events, delivery, candidate)?;
    let checkout = checkout_attestation_from_snapshot(&terminal, &snapshot);
    let (findings, evidence) = resolve_findings(
        delivery,
        candidate,
        &binding.id,
        &terminal,
        &checkout,
        &events,
        result,
    )?;

    Ok(ResolvedVerificationRuntime {
        role,
        session: VerificationSessionFacts {
            role,
            stage_run_id: run.id.clone(),
            session_binding_id: binding.id.clone(),
            workspace_mode: VerificationWorkspaceMode::CandidateReadOnly,
            permission_profile: VerificationPermissionProfile::CandidateReadOnlyRestricted,
            pre_candidate_snapshot: Some(snapshot.clone()),
            post_candidate_snapshot: Some(snapshot),
            accepted_job_outcome: Some(terminal),
            codex_turn_completed: true,
            mutation_records: Vec::new(),
            findings,
        },
        evidence,
        finished_at_millis: verified_terminal.finished_at_millis(),
    })
}

struct AcceptedRuntimeEvent<'event> {
    event: &'event ProductionRuntimeEvent,
    occurred_at_millis: u64,
    sequence: u64,
    payload: Option<Vec<u8>>,
}

fn validated_runtime_events<'events>(
    events: &'events [ProductionRuntimeEvent],
    terminal: &AcceptedVerificationJobOutcomeFact,
) -> Result<Vec<AcceptedRuntimeEvent<'events>>, ProductionVerdictResolutionError> {
    let terminal_sequence = u64::try_from(terminal.last_event_sequence().0)
        .map_err(|_| error("verification terminal sequence is invalid"))?;
    if events.is_empty()
        || events.len() > MAX_RUNTIME_EVENTS
        || events.len() as u64 != terminal_sequence
    {
        return Err(error(
            "verification runtime ledger is empty, truncated, or exceeds its terminal position",
        ));
    }
    let mut accepted = Vec::with_capacity(events.len());
    let mut ids = HashSet::with_capacity(events.len());
    for (index, durable) in events.iter().enumerate() {
        let sequence = durable.sequence;
        if sequence != index as u64 + 1 || !ids.insert(durable.event_id.0.as_str()) {
            return Err(error(
                "verification runtime ledger is non-contiguous or repeats an event identity",
            ));
        }
        if durable.occurred_at_millis > terminal.finished_at_millis() {
            return Err(error(
                "verification runtime event follows its accepted terminal outcome",
            ));
        }
        let payload = durable
            .payload
            .as_ref()
            .filter(|payload| payload.content_type == JSON_CONTENT_TYPE)
            .map(|payload| payload.bytes.clone());
        accepted.push(AcceptedRuntimeEvent {
            event: durable,
            occurred_at_millis: durable.occurred_at_millis,
            sequence,
            payload,
        });
    }
    Ok(accepted)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionPolicyPayload {
    protocol: String,
    workspace_mode: String,
    permission_profile: String,
    candidate_ref: String,
}

fn validate_policy_attestation(
    events: &[AcceptedRuntimeEvent<'_>],
    candidate: &FrozenDeliveryCandidate,
) -> Result<(), ProductionVerdictResolutionError> {
    let matching = events
        .iter()
        .filter(|event| event.event.category == ProductionRuntimeEventCategory::Lifecycle)
        .filter_map(|event| event.payload.as_deref())
        .filter_map(|payload| serde_json::from_slice::<SessionPolicyPayload>(payload).ok())
        .filter(|payload| payload.protocol == SESSION_POLICY_PROTOCOL)
        .collect::<Vec<_>>();
    let [policy] = matching.as_slice() else {
        return Err(error(
            "verification runtime must contain one exact read-only policy attestation",
        ));
    };
    if policy.workspace_mode != "candidate-read-only"
        || policy.permission_profile != "candidate-read-only-restricted"
        || policy.candidate_ref != candidate.candidate_ref()
    {
        return Err(error(
            "verification runtime policy or candidate checkout is stale",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerificationResultPayload {
    protocol: String,
    delivery_spec_id: DeliverySpecId,
    delivery_spec_revision: u64,
    candidate_ref: String,
    findings: Vec<StructuredFinding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredFinding {
    finding_id: String,
    criterion_id: Option<String>,
    verdict: StructuredVerdict,
    explanation: String,
    evidence_sources: Vec<StructuredEvidenceSource>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StructuredVerdict {
    Pass,
    Fail,
    Inconclusive,
    InfraError,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredEvidenceSource {
    #[serde(rename = "type")]
    evidence_type: EvidenceRefType,
    event_id: ExecutionEventId,
}

struct ParsedVerificationResult<'event> {
    event: &'event AcceptedRuntimeEvent<'event>,
    payload: VerificationResultPayload,
}

fn structured_result<'events>(
    events: &'events [AcceptedRuntimeEvent<'events>],
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
) -> Result<ParsedVerificationResult<'events>, ProductionVerdictResolutionError> {
    let matching = events
        .iter()
        .filter(|event| event.event.category == ProductionRuntimeEventCategory::Activity)
        .filter_map(|event| {
            event.payload.as_deref().and_then(|payload| {
                serde_json::from_slice::<VerificationResultPayload>(payload)
                    .ok()
                    .filter(|result| result.protocol == VERIFICATION_RESULT_PROTOCOL)
                    .map(|payload| ParsedVerificationResult { event, payload })
            })
        })
        .collect::<Vec<_>>();
    let [result] = matching.as_slice() else {
        return Err(error(
            "verification runtime must contain one canonical structured result",
        ));
    };
    if result.payload.delivery_spec_id != delivery.snapshot().spec.id
        || result.payload.delivery_spec_revision != delivery.snapshot().spec.revision
        || result.payload.candidate_ref != candidate.candidate_ref()
        || result.payload.findings.is_empty()
    {
        return Err(error(
            "verification result names a stale candidate, specification, or empty finding set",
        ));
    }
    Ok(ParsedVerificationResult {
        event: result.event,
        payload: VerificationResultPayload {
            protocol: result.payload.protocol.clone(),
            delivery_spec_id: result.payload.delivery_spec_id.clone(),
            delivery_spec_revision: result.payload.delivery_spec_revision,
            candidate_ref: result.payload.candidate_ref.clone(),
            findings: result.payload.findings.clone(),
        },
    })
}

impl Clone for StructuredFinding {
    fn clone(&self) -> Self {
        Self {
            finding_id: self.finding_id.clone(),
            criterion_id: self.criterion_id.clone(),
            verdict: self.verdict,
            explanation: self.explanation.clone(),
            evidence_sources: self.evidence_sources.clone(),
        }
    }
}

impl Clone for StructuredEvidenceSource {
    fn clone(&self) -> Self {
        Self {
            evidence_type: self.evidence_type,
            event_id: self.event_id.clone(),
        }
    }
}

impl Clone for VerificationResultPayload {
    fn clone(&self) -> Self {
        Self {
            protocol: self.protocol.clone(),
            delivery_spec_id: self.delivery_spec_id.clone(),
            delivery_spec_revision: self.delivery_spec_revision,
            candidate_ref: self.candidate_ref.clone(),
            findings: self.findings.clone(),
        }
    }
}

fn resolve_findings(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    session_binding_id: &SessionBindingId,
    terminal: &AcceptedVerificationJobOutcomeFact,
    checkout: &crate::domain::evidence::ValidatedCheckoutAttestationFact,
    events: &[AcceptedRuntimeEvent<'_>],
    result: ParsedVerificationResult<'_>,
) -> Result<
    (Vec<VerificationFindingFact>, Vec<ResolvedDeliveryEvidence>),
    ProductionVerdictResolutionError,
> {
    let mut findings = Vec::with_capacity(result.payload.findings.len());
    let mut evidence_by_source = BTreeMap::new();
    let mut criterion_ids = HashSet::with_capacity(result.payload.findings.len());
    for finding in result.payload.findings {
        let criterion_id = finding
            .criterion_id
            .ok_or_else(|| error("verification finding has no current criterion"))?;
        if !criterion_ids.insert(criterion_id.clone()) {
            return Err(error("verification result repeats one criterion"));
        }
        let conclusion = match finding.verdict {
            StructuredVerdict::Pass => VerificationFindingConclusion::Pass,
            StructuredVerdict::Fail => VerificationFindingConclusion::Fail,
            StructuredVerdict::Inconclusive | StructuredVerdict::InfraError => {
                return Err(error(
                    "a successful verification terminal must report pass or fail findings",
                ));
            }
        };
        if finding.evidence_sources.is_empty() {
            return Err(error("verification finding has no direct Evidence source"));
        }
        let mut source_refs = Vec::with_capacity(finding.evidence_sources.len());
        let mut source_sequences = Vec::with_capacity(finding.evidence_sources.len());
        for source in finding.evidence_sources {
            let source_event = exact_source_event(events, &source.event_id)?;
            if source_event.sequence >= result.event.sequence {
                return Err(error(
                    "verification finding Evidence must precede its structured result",
                ));
            }
            let outcome = runtime_outcome(source_event, source.evidence_type)?;
            let key = (
                source.event_id.0.clone(),
                evidence_type_order(source.evidence_type),
            );
            if !evidence_by_source.contains_key(&key) {
                let accepted = accepted_runtime_source(
                    terminal,
                    candidate,
                    source.event_id.clone(),
                    source.evidence_type,
                    source_event.sequence,
                    source_event.occurred_at_millis,
                    outcome,
                );
                let created_at_millis = terminal
                    .finished_at_millis()
                    .checked_add(1)
                    .ok_or_else(|| error("Evidence time exceeds the durable range"))?;
                let resolved = resolve_delivery_evidence(
                    delivery,
                    candidate,
                    ResolveDeliveryEvidenceInput {
                        stage_run_id: terminal.stage_run_id().clone(),
                        session_binding_id: session_binding_id.clone(),
                        source: EvidenceSource::Runtime {
                            evidence_type: source.evidence_type,
                            source_event_id: source.event_id.clone(),
                            accepted_sources: std::slice::from_ref(&accepted),
                            terminal,
                            checkout,
                        },
                        created_at_millis,
                    },
                )
                .map_err(|error_value| error(error_value.to_string()))?;
                evidence_by_source.insert(key.clone(), resolved);
            }
            source_refs.push(format!("runtime_event:{}", source.event_id.0));
            source_sequences.push(ExecutionSequence(
                i64::try_from(source_event.sequence)
                    .map_err(|_| error("Evidence source sequence exceeds the durable range"))?,
            ));
        }
        findings.push(VerificationFindingFact {
            finding_ref: finding.finding_id,
            criterion_id: AcceptanceCriterionId::new(criterion_id)
                .map_err(|error_value| resolution_error(&error_value))?,
            conclusion,
            result_sequence: ExecutionSequence(
                i64::try_from(result.event.sequence)
                    .map_err(|_| error("verification result sequence exceeds the durable range"))?,
            ),
            source_refs,
            source_sequences,
            explanation: finding.explanation,
        });
    }
    Ok((findings, evidence_by_source.into_values().collect()))
}

fn exact_source_event<'events>(
    events: &'events [AcceptedRuntimeEvent<'events>],
    event_id: &ExecutionEventId,
) -> Result<&'events AcceptedRuntimeEvent<'events>, ProductionVerdictResolutionError> {
    let mut matching = events
        .iter()
        .filter(|event| event.event.event_id == *event_id);
    let source = matching
        .next()
        .ok_or_else(|| error("verification finding Evidence source is missing"))?;
    if matching.next().is_some() {
        return Err(error(
            "verification finding Evidence source identity is ambiguous",
        ));
    }
    Ok(source)
}

fn runtime_outcome(
    event: &AcceptedRuntimeEvent<'_>,
    evidence_type: EvidenceRefType,
) -> Result<VerifiedEvidenceOutcome, ProductionVerdictResolutionError> {
    let expected_category = match evidence_type {
        EvidenceRefType::Test => ProductionRuntimeEventCategory::Test,
        EvidenceRefType::Command => ProductionRuntimeEventCategory::Command,
        _ => {
            return Err(error(
                "production verification currently accepts direct test or command Evidence",
            ));
        }
    };
    if event.event.category != expected_category {
        return Err(error(
            "verification Evidence type does not match its runtime category",
        ));
    }
    let payload = event
        .payload
        .as_deref()
        .ok_or_else(|| error("verification Evidence source has no structured outcome"))?;
    let value: Value = serde_json::from_slice(payload)
        .map_err(|_| error("verification Evidence source payload is not JSON"))?;
    let status = nested_string(&value, "status")
        .or_else(|| nested_string(&value, "outcome"))
        .map(|value| value.to_ascii_lowercase().replace('_', "-"));
    let exit_code = nested_i64(&value, "exit_code").or_else(|| nested_i64(&value, "exitCode"));
    let outcome = match status.as_deref() {
        Some("timed-out" | "timeout") => VerifiedEvidenceOutcome::TimedOut,
        Some("policy-denied" | "sandbox-denied" | "declined" | "denied") => {
            VerifiedEvidenceOutcome::PolicyDenied
        }
        Some("cancelled" | "canceled" | "interrupted") => VerifiedEvidenceOutcome::Cancelled,
        Some("infrastructure-error" | "infrastructure-failed") => {
            VerifiedEvidenceOutcome::InfrastructureFailed
        }
        Some("failed") => VerifiedEvidenceOutcome::Failed,
        Some("completed" | "succeeded" | "exited") if exit_code.unwrap_or(0) == 0 => {
            VerifiedEvidenceOutcome::Succeeded
        }
        _ if exit_code == Some(0) => VerifiedEvidenceOutcome::Succeeded,
        _ if exit_code.is_some_and(|code| code != 0) => VerifiedEvidenceOutcome::Failed,
        _ => VerifiedEvidenceOutcome::Observed,
    };
    Ok(outcome)
}

fn nested_string<'value>(value: &'value Value, key: &str) -> Option<&'value str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .or_else(|| value.get("result")?.get(key)?.as_str())
        .or_else(|| value.get("item")?.get(key)?.as_str())
        .or_else(|| value.get("evidence")?.get(key)?.as_str())
}

fn nested_i64(value: &Value, key: &str) -> Option<i64> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .or_else(|| value.get("result")?.get(key)?.as_i64())
        .or_else(|| value.get("item")?.get(key)?.as_i64())
        .or_else(|| value.get("evidence")?.get(key)?.as_i64())
}

fn required_roles(
    delivery: &Delivery,
) -> Result<Vec<VerificationRole>, ProductionVerdictResolutionError> {
    let roles = [
        VerificationRole::Reviewer,
        VerificationRole::Verifier,
        VerificationRole::AdversarialVerifier,
    ]
    .into_iter()
    .filter(|role| {
        delivery.snapshot().stage_runs.iter().any(|run| {
            run.stage == crate::domain::DeliveryStage::Verifying
                && verification_role(&run.role).ok() == Some(*role)
        })
    })
    .collect::<Vec<_>>();
    if !matches!(
        roles.as_slice(),
        [VerificationRole::Reviewer, VerificationRole::Verifier]
            | [
                VerificationRole::Reviewer,
                VerificationRole::Verifier,
                VerificationRole::AdversarialVerifier
            ]
    ) {
        return Err(error(
            "Delivery does not contain the canonical verification roles",
        ));
    }
    Ok(roles)
}

fn verification_role(value: &str) -> Result<VerificationRole, ProductionVerdictResolutionError> {
    match value {
        "reviewer" => Ok(VerificationRole::Reviewer),
        "verifier" => Ok(VerificationRole::Verifier),
        "adversarial-verifier" => Ok(VerificationRole::AdversarialVerifier),
        _ => Err(error("verification StageRun role is not canonical")),
    }
}

const fn role_order(role: VerificationRole) -> u8 {
    match role {
        VerificationRole::Reviewer => 0,
        VerificationRole::Verifier => 1,
        VerificationRole::AdversarialVerifier => 2,
    }
}

const fn evidence_type_order(evidence_type: EvidenceRefType) -> u8 {
    match evidence_type {
        EvidenceRefType::Test => 0,
        EvidenceRefType::Command => 1,
        EvidenceRefType::Diff => 2,
        EvidenceRefType::File => 3,
        EvidenceRefType::Commit => 4,
        EvidenceRefType::PullRequest => 5,
        EvidenceRefType::RuntimeEvent => 6,
        EvidenceRefType::ReviewFinding => 7,
    }
}

fn resolution_error(error_value: &DeliveryValidationError) -> ProductionVerdictResolutionError {
    error(error_value.to_string())
}

fn error(message: impl Into<String>) -> ProductionVerdictResolutionError {
    ProductionVerdictResolutionError {
        message: message.into(),
    }
}
