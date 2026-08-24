// SPDX-License-Identifier: Apache-2.0

//! Independent candidate verification facts.
//!
//! Runtime observations enter through explicit fact types. The validated
//! values returned by [`validate_independent_verification`] keep their fields
//! private, so a verdict can consume only assignments, outcomes, and findings
//! that were reconciled with the current Delivery and frozen candidate.

use std::collections::HashSet;

use serde::Serialize;
use winwincode_domain::{
    CodexThreadId, ExecutionAckSequence, ExecutionJobId, ExecutionSequence, FencingToken, LeaseId,
    ProductSessionId, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

use crate::application::stage::{
    TerminalArtifactReference, TerminalOutcomeStatus, VerifiedTerminalOutcome,
};

use super::candidate::assert_validated_git_snapshot_fact;
use super::{
    AcceptanceCriterionId, Delivery, DeliveryStage, DeliveryValidationError,
    DeliveryValidationErrorCode, FrozenDeliveryCandidate, MAX_REFERENCE_LENGTH, MAX_SAFE_INTEGER,
    MAX_TEXT_LENGTH, RepositoryRef, SessionBinding, SessionBindingId, StageRun, StageRunActorType,
    StageRunStatus, ValidatedGitSnapshotFact, assert_frozen_candidate_current, bounded_text,
    collection_length, portable_identifier, validation_error,
};

const MAX_EXECUTION_ATTEMPT: u64 = 1_000;

#[derive(PartialEq, Eq)]
struct FencedExecutionIdentity<'fact> {
    execution_job_id: &'fact ExecutionJobId,
    attempt: u64,
    lease_id: &'fact LeaseId,
    fencing_token: &'fact FencingToken,
    worker_id: &'fact WorkerId,
    worker_instance_id: &'fact WorkerInstanceId,
    worker_session_id: &'fact WorkerSessionId,
}

#[allow(dead_code)]
impl<'fact> FencedExecutionIdentity<'fact> {
    fn verified(outcome: &'fact VerifiedTerminalOutcome) -> Self {
        Self {
            execution_job_id: outcome.execution_job_id(),
            attempt: outcome.attempt(),
            lease_id: outcome.lease_id(),
            fencing_token: outcome.fencing_token(),
            worker_id: outcome.worker_id(),
            worker_instance_id: outcome.worker_instance_id(),
            worker_session_id: outcome.worker_session_id(),
        }
    }

    fn outcome(outcome: &'fact AcceptedVerificationJobOutcomeFact) -> Self {
        Self {
            execution_job_id: &outcome.execution_job_id,
            attempt: outcome.attempt,
            lease_id: &outcome.lease_id,
            fencing_token: &outcome.fencing_token,
            worker_id: &outcome.worker_id,
            worker_instance_id: &outcome.worker_instance_id,
            worker_session_id: &outcome.worker_session_id,
        }
    }

    fn snapshot(snapshot: &'fact ValidatedGitSnapshotFact) -> Self {
        Self {
            execution_job_id: snapshot.execution_job_id(),
            attempt: snapshot.attempt(),
            lease_id: snapshot.lease_id(),
            fencing_token: snapshot.fencing_token(),
            worker_id: snapshot.worker_id(),
            worker_instance_id: snapshot.worker_instance_id(),
            worker_session_id: snapshot.worker_session_id(),
        }
    }

    fn mutation(record: &'fact VerificationWorkerMutationRecord) -> Self {
        Self {
            execution_job_id: &record.execution_job_id,
            attempt: record.attempt,
            lease_id: &record.lease_id,
            fencing_token: &record.fencing_token,
            worker_id: &record.worker_id,
            worker_instance_id: &record.worker_instance_id,
            worker_session_id: &record.worker_session_id,
        }
    }
}

/// One canonical independent verification role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerificationRole {
    Reviewer,
    Verifier,
    AdversarialVerifier,
}

impl VerificationRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reviewer => "reviewer",
            Self::Verifier => "verifier",
            Self::AdversarialVerifier => "adversarial-verifier",
        }
    }
}

/// Workspace policy observed for one verification Session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub(crate) enum VerificationWorkspaceMode {
    CandidateReadOnly,
    CandidateWrite,
    SourceReadOnly,
}

/// Restricted permission profile accepted for an independent verifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub(crate) enum VerificationPermissionProfile {
    CandidateReadOnlyRestricted,
    Unrestricted,
}

/// Candidate mutation category emitted by the Worker runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum VerificationMutationKind {
    FileWrite,
    PatchApply,
}

/// Worker-authoritative mutation record under an exact fenced attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationWorkerMutationRecord {
    pub(crate) execution_job_id: ExecutionJobId,
    pub(crate) attempt: u64,
    pub(crate) lease_id: LeaseId,
    pub(crate) fencing_token: FencingToken,
    pub(crate) worker_id: WorkerId,
    pub(crate) worker_instance_id: WorkerInstanceId,
    pub(crate) worker_session_id: WorkerSessionId,
    pub(crate) sequence: ExecutionSequence,
    pub(crate) kind: VerificationMutationKind,
    pub(crate) succeeded: bool,
    pub(crate) resulting_candidate_tree_id: String,
}

impl VerificationWorkerMutationRecord {
    #[allow(dead_code)]
    pub(crate) fn from_terminal_outcome(
        outcome: &AcceptedVerificationJobOutcomeFact,
        sequence: ExecutionSequence,
        kind: VerificationMutationKind,
        succeeded: bool,
        resulting_candidate_tree_id: impl Into<String>,
    ) -> Self {
        Self {
            execution_job_id: outcome.execution_job_id.clone(),
            attempt: outcome.attempt,
            lease_id: outcome.lease_id.clone(),
            fencing_token: outcome.fencing_token.clone(),
            worker_id: outcome.worker_id.clone(),
            worker_instance_id: outcome.worker_instance_id.clone(),
            worker_session_id: outcome.worker_session_id.clone(),
            sequence,
            kind,
            succeeded,
            resulting_candidate_tree_id: resulting_candidate_tree_id.into(),
        }
    }
}

/// Terminal status from one Control-Plane-accepted Worker `JobOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationJobOutcomeStatus {
    Succeeded,
    Failed,
    InfrastructureError,
    Cancelled,
}

/// One role-reported criterion conclusion, before evidence sealing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationFindingConclusion {
    Pass,
    Fail,
}

/// Observed finding payload tied to one verification Session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationFindingFact {
    pub(crate) finding_ref: String,
    pub(crate) criterion_id: AcceptanceCriterionId,
    pub(crate) conclusion: VerificationFindingConclusion,
    pub(crate) result_sequence: ExecutionSequence,
    pub(crate) source_refs: Vec<String>,
    pub(crate) source_sequences: Vec<ExecutionSequence>,
    pub(crate) explanation: String,
}

/// Runtime facts for one role-scoped verification Session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationSessionFacts {
    pub(crate) role: VerificationRole,
    pub(crate) stage_run_id: StageRunId,
    pub(crate) session_binding_id: SessionBindingId,
    pub(crate) workspace_mode: VerificationWorkspaceMode,
    pub(crate) permission_profile: VerificationPermissionProfile,
    pub(crate) pre_candidate_snapshot: Option<ValidatedGitSnapshotFact>,
    pub(crate) post_candidate_snapshot: Option<ValidatedGitSnapshotFact>,
    pub(crate) accepted_job_outcome: Option<AcceptedVerificationJobOutcomeFact>,
    /// A Codex runtime event, intentionally insufficient for settlement.
    pub(crate) codex_turn_completed: bool,
    pub(crate) mutation_records: Vec<VerificationWorkerMutationRecord>,
    pub(crate) findings: Vec<VerificationFindingFact>,
}

/// Opaque, crate-assembled facts accepted by the verification seam.
///
/// The fields have crate visibility so Delivery adapters can assemble facts;
/// transport callers cannot construct this value or any sealed attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationFacts {
    pub(crate) required_roles: Vec<VerificationRole>,
    pub(crate) sessions: Vec<VerificationSessionFacts>,
}

/// One verified current role assignment derived from canonical Delivery facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationAssignment {
    role: VerificationRole,
    stage_run_id: StageRunId,
    session_binding_id: SessionBindingId,
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    repository: RepositoryRef,
    checkout_revision: String,
}

impl VerificationAssignment {
    pub fn role(&self) -> VerificationRole {
        self.role
    }

    pub fn stage_run_id(&self) -> &StageRunId {
        &self.stage_run_id
    }

    pub fn session_binding_id(&self) -> &SessionBindingId {
        &self.session_binding_id
    }

    pub fn product_session_id(&self) -> &ProductSessionId {
        &self.product_session_id
    }

    pub fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    pub fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }

    pub fn codex_thread_id(&self) -> &CodexThreadId {
        &self.codex_thread_id
    }

    pub fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub fn checkout_revision(&self) -> &str {
        &self.checkout_revision
    }
}

/// Fail-closed state derived from canonical and accepted runtime facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationSessionState {
    Missing,
    Running,
    Incomplete,
    Failed,
    Cancelled,
    Settled,
}

/// Constructor-derived terminal settlement for one verification role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationTerminalSettlement {
    Settled,
    Failed,
    InfrastructureError,
    Cancelled,
}

/// Validated terminal Worker outcome bound to one current assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedVerificationJobOutcomeFact {
    product_session_id: ProductSessionId,
    stage_run_id: StageRunId,
    role_id: String,
    execution_job_id: ExecutionJobId,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    last_event_sequence: ExecutionAckSequence,
    finished_at_millis: u64,
    status: VerificationJobOutcomeStatus,
    terminal_candidate_tree_id: String,
    artifacts: Vec<TerminalArtifactReference>,
}

#[allow(dead_code)]
impl AcceptedVerificationJobOutcomeFact {
    /// Joins the stage coordinator's sealed outcome to one adapter-sealed Job
    /// snapshot. No caller-supplied identity or terminal timestamp is copied.
    pub(crate) fn from_verified_outcome(
        outcome: &VerifiedTerminalOutcome,
        snapshot: &ValidatedGitSnapshotFact,
        role_id: impl Into<String>,
    ) -> Result<Self, DeliveryValidationError> {
        let role_id = role_id.into();
        portable_identifier(&role_id, "verification.terminal.roleId")?;
        let exact_artifact = outcome.artifacts().iter().any(|artifact| {
            artifact.artifact_id.0 == snapshot.artifact_ref()
                && &artifact.digest == snapshot.artifact_digest()
        });
        let exact_metadata = outcome.codex_thread_id() == Some(snapshot.codex_thread_id())
            && outcome.finished_at_millis() == snapshot.finished_at_millis()
            && u64::try_from(outcome.last_event_sequence().0).ok()
                == Some(snapshot.last_event_sequence())
            && exact_artifact;
        if FencedExecutionIdentity::verified(outcome) != FencedExecutionIdentity::snapshot(snapshot)
            || outcome.stage_run_id() != snapshot.stage_run_id()
            || !exact_metadata
        {
            return Err(relationship_mismatch(
                "verification.terminal",
                "verified terminal outcome and sealed Job snapshot identity, time, sequence, CodexThread, or Artifact differ",
            ));
        }
        let last_event_sequence = i64::try_from(snapshot.last_event_sequence()).map_err(|_| {
            invalid_verification(
                "verification.terminal.lastEventSequence",
                "snapshot sequence exceeds the supported safe integer range",
            )
        })?;
        Ok(Self {
            product_session_id: snapshot.product_session_id().clone(),
            stage_run_id: outcome.stage_run_id().clone(),
            role_id,
            execution_job_id: outcome.execution_job_id().clone(),
            attempt: outcome.attempt(),
            lease_id: outcome.lease_id().clone(),
            fencing_token: outcome.fencing_token().clone(),
            worker_id: outcome.worker_id().clone(),
            worker_instance_id: outcome.worker_instance_id().clone(),
            worker_session_id: outcome.worker_session_id().clone(),
            codex_thread_id: snapshot.codex_thread_id().clone(),
            last_event_sequence: ExecutionAckSequence(last_event_sequence),
            finished_at_millis: snapshot.finished_at_millis(),
            status: match outcome.status() {
                TerminalOutcomeStatus::Succeeded => VerificationJobOutcomeStatus::Succeeded,
                TerminalOutcomeStatus::Failed => VerificationJobOutcomeStatus::Failed,
                TerminalOutcomeStatus::InfrastructureError => {
                    VerificationJobOutcomeStatus::InfrastructureError
                }
                TerminalOutcomeStatus::Cancelled => VerificationJobOutcomeStatus::Cancelled,
            },
            terminal_candidate_tree_id: snapshot.candidate_tree_id().into(),
            artifacts: outcome.artifacts().to_vec(),
        })
    }

    pub(crate) fn product_session_id(&self) -> &ProductSessionId {
        &self.product_session_id
    }

    pub(crate) fn stage_run_id(&self) -> &StageRunId {
        &self.stage_run_id
    }

    pub(crate) fn role_id(&self) -> &str {
        &self.role_id
    }

    pub(crate) fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    pub(crate) fn attempt(&self) -> u64 {
        self.attempt
    }

    pub(crate) fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }

    pub(crate) fn fencing_token(&self) -> &FencingToken {
        &self.fencing_token
    }

    pub(crate) fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    pub(crate) fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.worker_instance_id
    }

    pub(crate) fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }

    pub(crate) fn codex_thread_id(&self) -> &CodexThreadId {
        &self.codex_thread_id
    }

    pub(crate) fn last_event_sequence(&self) -> &ExecutionAckSequence {
        &self.last_event_sequence
    }

    pub(crate) fn finished_at_millis(&self) -> u64 {
        self.finished_at_millis
    }

    pub(crate) fn status(&self) -> VerificationJobOutcomeStatus {
        self.status
    }

    pub(crate) fn terminal_candidate_tree_id(&self) -> &str {
        &self.terminal_candidate_tree_id
    }
}

/// Validated role finding bound to the current candidate and criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationFinding {
    role: VerificationRole,
    finding_ref: String,
    criterion_id: AcceptanceCriterionId,
    conclusion: VerificationFindingConclusion,
    source_refs: Vec<String>,
    result_sequence: ExecutionSequence,
    source_sequences: Vec<ExecutionSequence>,
    explanation: String,
    candidate_ref: String,
}

impl VerificationFinding {
    pub fn role(&self) -> VerificationRole {
        self.role
    }

    pub fn finding_ref(&self) -> &str {
        &self.finding_ref
    }

    pub fn criterion_id(&self) -> &AcceptanceCriterionId {
        &self.criterion_id
    }

    pub fn conclusion(&self) -> VerificationFindingConclusion {
        self.conclusion
    }

    pub fn source_refs(&self) -> &[String] {
        &self.source_refs
    }

    #[allow(dead_code)]
    pub(crate) fn result_sequence(&self) -> &ExecutionSequence {
        &self.result_sequence
    }

    #[allow(dead_code)]
    pub(crate) fn source_sequences(&self) -> &[ExecutionSequence] {
        &self.source_sequences
    }

    pub fn explanation(&self) -> &str {
        &self.explanation
    }

    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }
}

/// One required role and its constructor-derived state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationRoleSettlement {
    role: VerificationRole,
    state: VerificationSessionState,
    assignment: Option<VerificationAssignment>,
    findings: Vec<VerificationFinding>,
    terminal_job_outcome: Option<AcceptedVerificationJobOutcomeFact>,
    terminal_settlement: Option<VerificationTerminalSettlement>,
}

impl VerificationRoleSettlement {
    pub fn role(&self) -> VerificationRole {
        self.role
    }

    pub fn state(&self) -> VerificationSessionState {
        self.state
    }

    pub fn assignment(&self) -> Option<&VerificationAssignment> {
        self.assignment.as_ref()
    }

    pub fn findings(&self) -> &[VerificationFinding] {
        &self.findings
    }

    pub fn terminal_job_outcome(&self) -> Option<&AcceptedVerificationJobOutcomeFact> {
        self.terminal_job_outcome.as_ref()
    }

    pub fn terminal_settlement(&self) -> Option<VerificationTerminalSettlement> {
        self.terminal_settlement
    }
}

/// Derived independent-verification facts for one current candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndependentVerification {
    candidate_ref: String,
    settlements: Vec<VerificationRoleSettlement>,
}

impl IndependentVerification {
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    pub fn settlements(&self) -> &[VerificationRoleSettlement] {
        &self.settlements
    }
}

/// Validates role-scoped independent verification against one frozen candidate.
///
/// # Errors
///
/// Rejects stale candidates, assignment drift, mutable workspaces, unaccepted
/// terminal outcomes, stale lease identities, changed terminal trees, and any
/// successful file or patch mutation.
#[allow(clippy::too_many_lines)]
pub fn validate_independent_verification(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    facts: &VerificationFacts,
) -> Result<IndependentVerification, DeliveryValidationError> {
    assert_frozen_candidate_current(delivery, candidate)?;
    validate_required_roles(&facts.required_roles)?;
    if facts
        .sessions
        .iter()
        .any(|session| !facts.required_roles.contains(&session.role))
    {
        return Err(invalid_verification(
            "verification.sessions",
            "verification Session role was not requested",
        ));
    }

    let snapshot = delivery.snapshot();
    let writer_binding = snapshot
        .session_bindings
        .iter()
        .find(|binding| binding.id == *candidate.producer_session_binding_id())
        .ok_or_else(|| {
            relationship_mismatch(
                "verification.sessions",
                "candidate writer SessionBinding is missing",
            )
        })?;
    let writer_worker_session = writer_binding.worker_session_id.as_ref().ok_or_else(|| {
        relationship_mismatch(
            "verification.sessions",
            "candidate writer WorkerSession is missing",
        )
    })?;
    let writer_codex_thread = writer_binding.codex_thread_id.as_ref().ok_or_else(|| {
        relationship_mismatch(
            "verification.sessions",
            "candidate writer CodexThread is missing",
        )
    })?;

    let mut stage_run_ids = HashSet::from([candidate.producer_stage_run_id().0.clone()]);
    let mut binding_ids = HashSet::from([candidate.producer_session_binding_id().0.clone()]);
    let mut product_session_ids = HashSet::from([writer_binding.product_session_id.0.clone()]);
    let mut execution_job_ids = HashSet::from([writer_binding.execution_job_id.0.clone()]);
    let mut worker_session_ids = HashSet::from([writer_worker_session.0.clone()]);
    let mut codex_thread_ids = HashSet::from([writer_codex_thread.0.clone()]);
    let mut settlements = Vec::with_capacity(facts.required_roles.len());

    for (index, required_role) in facts.required_roles.iter().copied().enumerate() {
        let path = format!("verification.sessions[{index}]");
        let matching: Vec<_> = facts
            .sessions
            .iter()
            .filter(|session| session.role == required_role)
            .collect();
        if matching.len() > 1 {
            return Err(relationship_mismatch(
                &path,
                "verification role must have exactly one current Session fact",
            ));
        }
        let run = current_role_run(delivery, required_role, &path)?;
        validate_role_consumes_candidate(delivery, run, candidate, &path)?;
        let binding = current_role_binding(delivery, run, &path)?;
        ensure_distinct_assignment(
            run,
            binding,
            &path,
            &mut stage_run_ids,
            &mut binding_ids,
            &mut product_session_ids,
            &mut execution_job_ids,
            &mut worker_session_ids,
            &mut codex_thread_ids,
        )?;
        let Some(session) = matching.first().copied() else {
            settlements.push(missing_settlement(required_role));
            continue;
        };
        if session.workspace_mode != VerificationWorkspaceMode::CandidateReadOnly
            || session.permission_profile
                != VerificationPermissionProfile::CandidateReadOnlyRestricted
        {
            return Err(invalid_verification(
                &format!("{path}.workspacePolicy"),
                "verification workspace must use candidate-read-only mode and its restricted permission profile",
            ));
        }

        if run.id != session.stage_run_id {
            return Err(relationship_mismatch(
                &path,
                "verification Session does not reference the role's current StageRun",
            ));
        }
        if binding.id != session.session_binding_id {
            return Err(relationship_mismatch(
                &path,
                "current verification StageRun does not match the referenced SessionBinding",
            ));
        }
        let assignment = validate_assignment(binding, run, session, candidate, &path)?;
        let terminal_job_outcome = validate_terminal_job_outcome(
            run,
            binding,
            assignment.as_ref(),
            session,
            candidate,
            &path,
        )?;
        let findings = validate_findings(
            delivery,
            candidate,
            session,
            terminal_job_outcome.as_ref(),
            &path,
        )?;
        let (state, terminal_settlement) = derive_session_state(
            run,
            assignment.as_ref(),
            terminal_job_outcome.as_ref(),
            &findings,
            &path,
        )?;
        settlements.push(VerificationRoleSettlement {
            role: required_role,
            state,
            assignment,
            findings,
            terminal_job_outcome,
            terminal_settlement,
        });
    }

    Ok(IndependentVerification {
        candidate_ref: candidate.candidate_ref().into(),
        settlements,
    })
}

fn validate_required_roles(roles: &[VerificationRole]) -> Result<(), DeliveryValidationError> {
    if matches!(
        roles,
        [VerificationRole::Reviewer, VerificationRole::Verifier]
            | [
                VerificationRole::Reviewer,
                VerificationRole::Verifier,
                VerificationRole::AdversarialVerifier
            ]
    ) {
        Ok(())
    } else {
        Err(invalid_verification(
            "verification.requiredRoles",
            "required roles must be reviewer, verifier, with an optional appended adversarial-verifier",
        ))
    }
}

fn missing_settlement(role: VerificationRole) -> VerificationRoleSettlement {
    VerificationRoleSettlement {
        role,
        state: VerificationSessionState::Missing,
        assignment: None,
        findings: vec![],
        terminal_job_outcome: None,
        terminal_settlement: None,
    }
}

fn current_role_run<'delivery>(
    delivery: &'delivery Delivery,
    role: VerificationRole,
    path: &str,
) -> Result<&'delivery StageRun, DeliveryValidationError> {
    let role_runs: Vec<_> = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| {
            run.stage == DeliveryStage::Verifying
                && run.actor_type == StageRunActorType::Codex
                && run.role == role.as_str()
        })
        .collect();
    let current_attempt = role_runs
        .iter()
        .map(|run| run.attempt)
        .max()
        .ok_or_else(|| {
            relationship_mismatch(path, "verification role has no verifying Codex StageRun")
        })?;
    let current: Vec<_> = role_runs
        .into_iter()
        .filter(|run| run.attempt == current_attempt)
        .collect();
    if current.len() != 1 {
        return Err(relationship_mismatch(
            path,
            "verification role must have exactly one current StageRun assignment",
        ));
    }
    Ok(current[0])
}

fn current_role_binding<'delivery>(
    delivery: &'delivery Delivery,
    run: &StageRun,
    path: &str,
) -> Result<&'delivery SessionBinding, DeliveryValidationError> {
    let bindings: Vec<_> = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == run.id)
        .collect();
    if bindings.len() != 1 {
        return Err(relationship_mismatch(
            path,
            "current verification StageRun must have exactly one SessionBinding",
        ));
    }
    let binding = bindings[0];
    let matches = binding.delivery_id == *delivery.id()
        && binding.delivery_task_id == run.delivery_task_id
        && binding.bound_at_millis >= run.started_at_millis
        && run
            .finished_at_millis
            .is_none_or(|finished| binding.bound_at_millis <= finished);
    if !matches {
        return Err(relationship_mismatch(
            path,
            "verification StageRun, task scope, and SessionBinding must match exactly",
        ));
    }
    Ok(binding)
}

fn validate_role_consumes_candidate(
    delivery: &Delivery,
    run: &StageRun,
    candidate: &FrozenDeliveryCandidate,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    let producer = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|producer| producer.id == *candidate.producer_stage_run_id())
        .ok_or_else(|| relationship_mismatch(path, "candidate producer StageRun is missing"))?;
    let producer_finished = producer.finished_at_millis.ok_or_else(|| {
        relationship_mismatch(path, "candidate producer StageRun has not finished")
    })?;
    if run.delivery_task_id != producer.delivery_task_id
        || run.started_at_millis < producer_finished
    {
        return Err(relationship_mismatch(
            path,
            "verification assignment must consume the current candidate in the same task scope",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ensure_distinct_assignment(
    run: &StageRun,
    binding: &SessionBinding,
    path: &str,
    stage_run_ids: &mut HashSet<String>,
    binding_ids: &mut HashSet<String>,
    product_session_ids: &mut HashSet<String>,
    execution_job_ids: &mut HashSet<String>,
    worker_session_ids: &mut HashSet<String>,
    codex_thread_ids: &mut HashSet<String>,
) -> Result<(), DeliveryValidationError> {
    let distinct = stage_run_ids.insert(run.id.0.clone())
        && binding_ids.insert(binding.id.0.clone())
        && product_session_ids.insert(binding.product_session_id.0.clone())
        && execution_job_ids.insert(binding.execution_job_id.0.clone())
        && binding
            .worker_session_id
            .as_ref()
            .is_none_or(|id| worker_session_ids.insert(id.0.clone()))
        && binding
            .codex_thread_id
            .as_ref()
            .is_none_or(|id| codex_thread_ids.insert(id.0.clone()));
    if distinct {
        Ok(())
    } else {
        Err(relationship_mismatch(
            path,
            "candidate writer and verification roles must use pairwise-distinct StageRun, SessionBinding, ProductSession, ExecutionJob, WorkerSession, and CodexThread identities",
        ))
    }
}

fn validate_assignment(
    binding: &SessionBinding,
    run: &StageRun,
    session: &VerificationSessionFacts,
    candidate: &FrozenDeliveryCandidate,
    path: &str,
) -> Result<Option<VerificationAssignment>, DeliveryValidationError> {
    let Some((worker_session_id, codex_thread_id, checkout)) = binding
        .worker_session_id
        .as_ref()
        .zip(binding.codex_thread_id.as_ref())
        .zip(session.pre_candidate_snapshot.as_ref())
        .map(|((worker, thread), checkout)| (worker, thread, checkout))
    else {
        return Ok(None);
    };
    assert_validated_git_snapshot_fact(checkout)?;
    if !snapshot_matches_assignment(checkout, run, binding)
        || !snapshot_matches_candidate(checkout, candidate)
    {
        return Err(relationship_mismatch(
            &format!("{path}.candidateCheckout"),
            "checkout must bind the complete current assignment to every frozen candidate Git fact",
        ));
    }
    Ok(Some(VerificationAssignment {
        role: session.role,
        stage_run_id: run.id.clone(),
        session_binding_id: binding.id.clone(),
        product_session_id: binding.product_session_id.clone(),
        execution_job_id: binding.execution_job_id.clone(),
        worker_session_id: worker_session_id.clone(),
        codex_thread_id: codex_thread_id.clone(),
        repository: checkout.repository().clone(),
        checkout_revision: checkout.candidate_commit_id().into(),
    }))
}

fn snapshot_matches_assignment(
    snapshot: &ValidatedGitSnapshotFact,
    run: &StageRun,
    binding: &SessionBinding,
) -> bool {
    snapshot.stage_run_id() == &run.id
        && snapshot.session_binding_id() == &binding.id
        && snapshot.product_session_id() == &binding.product_session_id
        && snapshot.execution_job_id() == &binding.execution_job_id
        && snapshot.attempt() == run.attempt
        && binding.worker_session_id.as_ref() == Some(snapshot.worker_session_id())
        && binding.codex_thread_id.as_ref() == Some(snapshot.codex_thread_id())
}

fn snapshot_matches_candidate(
    snapshot: &ValidatedGitSnapshotFact,
    candidate: &FrozenDeliveryCandidate,
) -> bool {
    snapshot.repository() == candidate.repository()
        && snapshot.base_commit_id() == candidate.base_commit_id()
        && snapshot.base_tree_id() == candidate.base_tree_id()
        && snapshot.candidate_commit_id() == candidate.candidate_commit_id()
        && snapshot.candidate_tree_id() == candidate.candidate_tree_id()
        && snapshot.diff_sha256() == candidate.diff_sha256()
        && snapshot.changed_paths() == candidate.changed_paths()
}

fn validate_terminal_job_outcome(
    run: &StageRun,
    binding: &SessionBinding,
    assignment: Option<&VerificationAssignment>,
    session: &VerificationSessionFacts,
    candidate: &FrozenDeliveryCandidate,
    path: &str,
) -> Result<Option<AcceptedVerificationJobOutcomeFact>, DeliveryValidationError> {
    let Some(outcome) = session.accepted_job_outcome.as_ref() else {
        if session.mutation_records.is_empty() && session.post_candidate_snapshot.is_none() {
            return Ok(None);
        }
        return Err(relationship_mismatch(
            &format!("{path}.mutationRecords"),
            "post-run snapshots and Worker mutation records require an accepted terminal JobOutcome",
        ));
    };
    let Some(assignment) = assignment else {
        return Err(relationship_mismatch(
            &format!("{path}.acceptedJobOutcome"),
            "accepted JobOutcome requires a complete assignment and candidate checkout",
        ));
    };
    validate_outcome_shape(outcome, path)?;
    let identity_matches = outcome.stage_run_id == run.id
        && outcome.product_session_id == binding.product_session_id
        && outcome.role_id == session.role.as_str()
        && outcome.execution_job_id == binding.execution_job_id
        && outcome.attempt == run.attempt
        && outcome.worker_session_id == *assignment.worker_session_id()
        && outcome.codex_thread_id == *assignment.codex_thread_id();
    if !identity_matches {
        return Err(relationship_mismatch(
            &format!("{path}.acceptedJobOutcome"),
            "accepted JobOutcome does not match its role, StageRun, ProductSession, ExecutionJob, attempt, WorkerSession, or CodexThread assignment",
        ));
    }
    if outcome.finished_at_millis < binding.bound_at_millis
        || run
            .finished_at_millis
            .is_some_and(|finished| outcome.finished_at_millis > finished)
    {
        return Err(relationship_mismatch(
            &format!("{path}.acceptedJobOutcome.finishedAtMillis"),
            "accepted JobOutcome finish time must be within its bound StageRun",
        ));
    }
    if outcome.terminal_candidate_tree_id != candidate.candidate_tree_id() {
        return Err(invalid_verification(
            &format!("{path}.acceptedJobOutcome.terminalCandidateTreeId"),
            "verification terminal tree must equal the frozen candidate tree",
        ));
    }
    validate_snapshot_pair(run, binding, session, outcome, candidate, path)?;
    validate_mutation_records(&session.mutation_records, outcome, path)?;

    Ok(Some(outcome.clone()))
}

fn validate_outcome_shape(
    outcome: &AcceptedVerificationJobOutcomeFact,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    portable_identifier(
        &outcome.product_session_id.0,
        &format!("{path}.acceptedJobOutcome.productSessionId"),
    )?;
    portable_identifier(
        &outcome.stage_run_id.0,
        &format!("{path}.acceptedJobOutcome.stageRunId"),
    )?;
    portable_identifier(
        &outcome.role_id,
        &format!("{path}.acceptedJobOutcome.roleId"),
    )?;
    portable_identifier(
        &outcome.execution_job_id.0,
        &format!("{path}.acceptedJobOutcome.executionJobId"),
    )?;
    portable_identifier(
        &outcome.lease_id.0,
        &format!("{path}.acceptedJobOutcome.leaseId"),
    )?;
    portable_identifier(
        &outcome.worker_id.0,
        &format!("{path}.acceptedJobOutcome.workerId"),
    )?;
    portable_identifier(
        &outcome.worker_instance_id.0,
        &format!("{path}.acceptedJobOutcome.workerInstanceId"),
    )?;
    portable_identifier(
        &outcome.worker_session_id.0,
        &format!("{path}.acceptedJobOutcome.workerSessionId"),
    )?;
    portable_identifier(
        &outcome.codex_thread_id.0,
        &format!("{path}.acceptedJobOutcome.codexThreadId"),
    )?;
    if !(1..=MAX_EXECUTION_ATTEMPT).contains(&outcome.attempt) {
        return Err(invalid_verification(
            &format!("{path}.acceptedJobOutcome.attempt"),
            "execution attempt must be between 1 and 1000",
        ));
    }
    if !valid_fencing_token(&outcome.fencing_token.0) {
        return Err(invalid_verification(
            &format!("{path}.acceptedJobOutcome.fencingToken"),
            "fencing token must be a positive decimal string of at most 20 digits",
        ));
    }
    if !(0..=i64::try_from(MAX_SAFE_INTEGER).unwrap_or(i64::MAX))
        .contains(&outcome.last_event_sequence.0)
    {
        return Err(invalid_verification(
            &format!("{path}.acceptedJobOutcome.lastEventSequence"),
            "last event sequence must be a non-negative safe integer",
        ));
    }
    if outcome.finished_at_millis > MAX_SAFE_INTEGER {
        return Err(invalid_verification(
            &format!("{path}.acceptedJobOutcome.finishedAtMillis"),
            "terminal finish time must be a non-negative safe integer",
        ));
    }
    if !git_object_id(&outcome.terminal_candidate_tree_id) {
        return Err(invalid_verification(
            &format!("{path}.acceptedJobOutcome.terminalCandidateTreeId"),
            "terminal candidate tree must be a lowercase Git object identity",
        ));
    }
    Ok(())
}

fn validate_mutation_records(
    records: &[VerificationWorkerMutationRecord],
    outcome: &AcceptedVerificationJobOutcomeFact,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    collection_length(records.len(), &format!("{path}.mutationRecords"))?;
    let mut sequences = HashSet::with_capacity(records.len());
    for (index, record) in records.iter().enumerate() {
        let record_path = format!("{path}.mutationRecords[{index}]");
        let identity_matches =
            FencedExecutionIdentity::mutation(record) == FencedExecutionIdentity::outcome(outcome);
        if !identity_matches {
            return Err(relationship_mismatch(
                &record_path,
                "Worker mutation record does not match the accepted job, attempt, lease, fence, Worker, or WorkerSession",
            ));
        }
        if record.sequence.0 <= 0
            || record.sequence.0 > outcome.last_event_sequence.0
            || !sequences.insert(record.sequence.0)
        {
            return Err(relationship_mismatch(
                &format!("{record_path}.sequence"),
                "Worker mutation sequence must be unique and covered by the accepted terminal outcome",
            ));
        }
        if record.resulting_candidate_tree_id != outcome.terminal_candidate_tree_id {
            return Err(relationship_mismatch(
                &format!("{record_path}.resultingCandidateTreeId"),
                "Worker mutation record must reconcile with the terminal candidate tree",
            ));
        }
        if record.succeeded {
            return Err(invalid_verification(
                &record_path,
                "a successful verification file write or patch invalidates the result",
            ));
        }
    }
    Ok(())
}

fn validate_snapshot_pair(
    run: &StageRun,
    binding: &SessionBinding,
    session: &VerificationSessionFacts,
    outcome: &AcceptedVerificationJobOutcomeFact,
    candidate: &FrozenDeliveryCandidate,
    path: &str,
) -> Result<(), DeliveryValidationError> {
    let Some((before, after)) = session
        .pre_candidate_snapshot
        .as_ref()
        .zip(session.post_candidate_snapshot.as_ref())
    else {
        return Err(relationship_mismatch(
            &format!("{path}.candidateSnapshots"),
            "settlement requires sealed pre- and post-verification Git snapshots",
        ));
    };
    assert_validated_git_snapshot_fact(before)?;
    assert_validated_git_snapshot_fact(after)?;
    let terminal_sequence = u64::try_from(outcome.last_event_sequence.0).unwrap_or_default();
    let exact_runtime_identity = [before, after].iter().all(|snapshot| {
        FencedExecutionIdentity::snapshot(snapshot) == FencedExecutionIdentity::outcome(outcome)
            && snapshot_matches_assignment(snapshot, run, binding)
            && terminal_names_snapshot(outcome, snapshot)
    });
    let exact_candidate = [before, after]
        .iter()
        .all(|snapshot| snapshot_matches_candidate(snapshot, candidate));
    let same_git_facts = before.repository() == after.repository()
        && before.base_commit_id() == after.base_commit_id()
        && before.base_tree_id() == after.base_tree_id()
        && before.candidate_commit_id() == after.candidate_commit_id()
        && before.candidate_tree_id() == after.candidate_tree_id()
        && before.diff_sha256() == after.diff_sha256()
        && before.changed_paths() == after.changed_paths()
        && before.changed_hunks() == after.changed_hunks();
    let observations_are_ordered = run.started_at_millis <= before.finished_at_millis()
        && binding.bound_at_millis <= before.finished_at_millis()
        && before.finished_at_millis() <= after.finished_at_millis()
        && after.finished_at_millis() <= outcome.finished_at_millis
        && before.last_event_sequence() <= after.last_event_sequence()
        && after.last_event_sequence() <= terminal_sequence;
    if !exact_runtime_identity || !exact_candidate || !same_git_facts || !observations_are_ordered {
        return Err(relationship_mismatch(
            &format!("{path}.candidateSnapshots"),
            "sealed pre/post snapshots must be ordered observations of one terminal fenced Job and unchanged frozen candidate",
        ));
    }
    Ok(())
}

fn terminal_names_snapshot(
    outcome: &AcceptedVerificationJobOutcomeFact,
    snapshot: &ValidatedGitSnapshotFact,
) -> bool {
    outcome.artifacts.iter().any(|artifact| {
        artifact.artifact_id.0 == snapshot.artifact_ref()
            && &artifact.digest == snapshot.artifact_digest()
    })
}

fn validate_findings(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    session: &VerificationSessionFacts,
    outcome: Option<&AcceptedVerificationJobOutcomeFact>,
    path: &str,
) -> Result<Vec<VerificationFinding>, DeliveryValidationError> {
    let Some(outcome) = outcome else {
        return Ok(vec![]);
    };
    collection_length(session.findings.len(), &format!("{path}.findings"))?;
    let criterion_ids: HashSet<_> = delivery
        .snapshot()
        .spec
        .acceptance_criteria
        .iter()
        .map(|criterion| criterion.id.0.as_str())
        .collect();
    let mut finding_refs = HashSet::with_capacity(session.findings.len());
    let mut findings = Vec::with_capacity(session.findings.len());
    for (index, finding) in session.findings.iter().enumerate() {
        let finding_path = format!("{path}.findings[{index}]");
        bounded_text(
            &finding.finding_ref,
            &format!("{finding_path}.findingRef"),
            MAX_REFERENCE_LENGTH,
        )?;
        if !finding_refs.insert(finding.finding_ref.as_str()) {
            return Err(relationship_mismatch(
                &format!("{path}.findings"),
                "verification finding identities must be unique per role",
            ));
        }
        if !criterion_ids.contains(finding.criterion_id.0.as_str()) {
            return Err(relationship_mismatch(
                &format!("{finding_path}.criterionId"),
                "verification finding criterion is not current",
            ));
        }
        if finding.result_sequence.0 <= 0
            || finding.result_sequence.0 > outcome.last_event_sequence.0
            || finding.source_sequences.len() != finding.source_refs.len()
        {
            return Err(relationship_mismatch(
                &format!("{finding_path}.resultSequence"),
                "finding result and source sequences must be covered by the terminal Worker outcome",
            ));
        }
        collection_length(
            finding.source_refs.len(),
            &format!("{finding_path}.sourceRefs"),
        )?;
        if finding.source_refs.is_empty() {
            return Err(invalid_verification(
                &format!("{finding_path}.sourceRefs"),
                "verification finding must cite at least one direct source",
            ));
        }
        let mut source_refs = HashSet::with_capacity(finding.source_refs.len());
        for (source_index, (source_ref, source_sequence)) in finding
            .source_refs
            .iter()
            .zip(&finding.source_sequences)
            .enumerate()
        {
            bounded_text(
                source_ref,
                &format!("{finding_path}.sourceRefs[{source_index}]"),
                MAX_REFERENCE_LENGTH,
            )?;
            if !source_refs.insert(source_ref.as_str()) {
                return Err(relationship_mismatch(
                    &format!("{finding_path}.sourceRefs"),
                    "verification finding source references must be unique",
                ));
            }
            if source_sequence.0 <= 0 || source_sequence.0 >= finding.result_sequence.0 {
                return Err(relationship_mismatch(
                    &format!("{finding_path}.sourceSequences[{source_index}]"),
                    "finding sources must precede the structured result sequence",
                ));
            }
        }
        bounded_text(
            &finding.explanation,
            &format!("{finding_path}.explanation"),
            MAX_TEXT_LENGTH,
        )?;
        findings.push(VerificationFinding {
            role: session.role,
            finding_ref: finding.finding_ref.clone(),
            criterion_id: finding.criterion_id.clone(),
            conclusion: finding.conclusion,
            source_refs: finding.source_refs.clone(),
            result_sequence: finding.result_sequence.clone(),
            source_sequences: finding.source_sequences.clone(),
            explanation: finding.explanation.clone(),
            candidate_ref: candidate.candidate_ref().into(),
        });
    }
    Ok(findings)
}

fn derive_session_state(
    run: &StageRun,
    assignment: Option<&VerificationAssignment>,
    outcome: Option<&AcceptedVerificationJobOutcomeFact>,
    findings: &[VerificationFinding],
    path: &str,
) -> Result<
    (
        VerificationSessionState,
        Option<VerificationTerminalSettlement>,
    ),
    DeliveryValidationError,
> {
    match (
        run.status,
        outcome.map(AcceptedVerificationJobOutcomeFact::status),
    ) {
        (StageRunStatus::Running | StageRunStatus::Waiting, None) => {
            Ok((VerificationSessionState::Running, None))
        }
        (StageRunStatus::Running | StageRunStatus::Waiting, Some(_)) => Err(relationship_mismatch(
            &format!("{path}.acceptedJobOutcome"),
            "an active StageRun cannot have an accepted terminal JobOutcome",
        )),
        (StageRunStatus::Succeeded | StageRunStatus::Failed | StageRunStatus::Cancelled, None) => {
            Ok((VerificationSessionState::Incomplete, None))
        }
        (StageRunStatus::Succeeded, Some(VerificationJobOutcomeStatus::Succeeded))
            if assignment.is_some() && !findings.is_empty() =>
        {
            Ok((
                VerificationSessionState::Settled,
                Some(VerificationTerminalSettlement::Settled),
            ))
        }
        (StageRunStatus::Succeeded, Some(VerificationJobOutcomeStatus::Succeeded)) => Ok((
            VerificationSessionState::Incomplete,
            Some(VerificationTerminalSettlement::Settled),
        )),
        (StageRunStatus::Failed, Some(VerificationJobOutcomeStatus::Failed)) => Ok((
            VerificationSessionState::Failed,
            Some(VerificationTerminalSettlement::Failed),
        )),
        (StageRunStatus::Failed, Some(VerificationJobOutcomeStatus::InfrastructureError)) => Ok((
            VerificationSessionState::Failed,
            Some(VerificationTerminalSettlement::InfrastructureError),
        )),
        (StageRunStatus::Cancelled, Some(VerificationJobOutcomeStatus::Cancelled)) => Ok((
            VerificationSessionState::Cancelled,
            Some(VerificationTerminalSettlement::Cancelled),
        )),
        _ => Err(relationship_mismatch(
            &format!("{path}.acceptedJobOutcome.status"),
            "accepted terminal JobOutcome status does not match the terminal StageRun",
        )),
    }
}

fn valid_fencing_token(value: &str) -> bool {
    value.len() <= 20
        && value
            .bytes()
            .next()
            .is_some_and(|byte| matches!(byte, b'1'..=b'9'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn invalid_verification(path: &str, message: &str) -> DeliveryValidationError {
    validation_error(DeliveryValidationErrorCode::InvalidValue, path, message)
}

fn relationship_mismatch(path: &str, message: &str) -> DeliveryValidationError {
    validation_error(
        DeliveryValidationErrorCode::RelationshipMismatch,
        path,
        message,
    )
}

/// Narrow sealed fixtures shared by sibling domain-module tests.
///
/// These helpers deliberately construct only the validated projection. They
/// are unavailable to production code and keep every production field private.
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support {
    #![allow(dead_code, clippy::wildcard_imports)]

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum VerificationFixtureState {
        Missing,
        Running,
        Incomplete,
        Failed,
        InfrastructureFailed,
        Cancelled,
        SettledPass,
        SettledFail,
    }

    pub(crate) fn independent_verification(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
        reviewer: VerificationFixtureState,
        verifier: VerificationFixtureState,
    ) -> IndependentVerification {
        assert_frozen_candidate_current(delivery, candidate)
            .expect("verification fixture requires the current candidate");
        let criterion_ids = delivery
            .snapshot()
            .spec
            .acceptance_criteria
            .iter()
            .map(|criterion| criterion.id.clone())
            .collect::<Vec<_>>();
        assert!(
            !criterion_ids.is_empty(),
            "verification fixture requires one acceptance criterion"
        );
        IndependentVerification {
            candidate_ref: candidate.candidate_ref().into(),
            settlements: vec![
                fixture_settlement(
                    delivery,
                    candidate,
                    &criterion_ids,
                    VerificationRole::Reviewer,
                    reviewer,
                ),
                fixture_settlement(
                    delivery,
                    candidate,
                    &criterion_ids,
                    VerificationRole::Verifier,
                    verifier,
                ),
            ],
        }
    }

    #[allow(clippy::too_many_lines)]
    fn fixture_settlement(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
        criterion_ids: &[AcceptanceCriterionId],
        role: VerificationRole,
        state: VerificationFixtureState,
    ) -> VerificationRoleSettlement {
        if state == VerificationFixtureState::Missing {
            return missing_settlement(role);
        }
        let role_id = role.as_str();
        let run = current_role_run(delivery, role, "verification.fixture")
            .expect("verification fixture requires one current role StageRun");
        let bindings = delivery
            .snapshot()
            .session_bindings
            .iter()
            .filter(|binding| binding.stage_run_id == run.id)
            .collect::<Vec<_>>();
        assert_eq!(
            bindings.len(),
            1,
            "verification fixture requires one current role SessionBinding"
        );
        let binding = bindings[0];
        let worker_session_id = binding
            .worker_session_id
            .clone()
            .expect("verification fixture requires one WorkerSession");
        let codex_thread_id = binding
            .codex_thread_id
            .clone()
            .expect("verification fixture requires one CodexThread");
        let assignment = VerificationAssignment {
            role,
            stage_run_id: run.id.clone(),
            session_binding_id: binding.id.clone(),
            product_session_id: binding.product_session_id.clone(),
            execution_job_id: binding.execution_job_id.clone(),
            worker_session_id: worker_session_id.clone(),
            codex_thread_id: codex_thread_id.clone(),
            repository: candidate.repository().clone(),
            checkout_revision: candidate.candidate_commit_id().into(),
        };
        let outcome_status = match state {
            VerificationFixtureState::Failed => VerificationJobOutcomeStatus::Failed,
            VerificationFixtureState::InfrastructureFailed => {
                VerificationJobOutcomeStatus::InfrastructureError
            }
            VerificationFixtureState::Cancelled => VerificationJobOutcomeStatus::Cancelled,
            VerificationFixtureState::Running
            | VerificationFixtureState::Incomplete
            | VerificationFixtureState::SettledPass
            | VerificationFixtureState::SettledFail => VerificationJobOutcomeStatus::Succeeded,
            VerificationFixtureState::Missing => unreachable!("handled above"),
        };
        let terminal = AcceptedVerificationJobOutcomeFact {
            product_session_id: binding.product_session_id.clone(),
            stage_run_id: run.id.clone(),
            role_id: role_id.into(),
            execution_job_id: binding.execution_job_id.clone(),
            attempt: run.attempt,
            lease_id: LeaseId(format!("lease-evidence-{}", run.id.0)),
            fencing_token: FencingToken("1".into()),
            worker_id: WorkerId("worker-evidence".into()),
            worker_instance_id: WorkerInstanceId("worker-instance-evidence".into()),
            worker_session_id,
            codex_thread_id,
            last_event_sequence: ExecutionAckSequence(2),
            finished_at_millis: run
                .finished_at_millis
                .expect("verification fixture requires a terminal StageRun"),
            status: outcome_status,
            terminal_candidate_tree_id: candidate.candidate_tree_id().into(),
            artifacts: vec![],
        };
        let (session_state, terminal_job_outcome, terminal_settlement, findings) = match state {
            VerificationFixtureState::Running => {
                (VerificationSessionState::Running, None, None, vec![])
            }
            VerificationFixtureState::Incomplete => (
                VerificationSessionState::Incomplete,
                Some(terminal),
                Some(VerificationTerminalSettlement::Settled),
                vec![],
            ),
            VerificationFixtureState::Failed => (
                VerificationSessionState::Failed,
                Some(terminal),
                Some(VerificationTerminalSettlement::Failed),
                vec![],
            ),
            VerificationFixtureState::InfrastructureFailed => (
                VerificationSessionState::Failed,
                Some(terminal),
                Some(VerificationTerminalSettlement::InfrastructureError),
                vec![],
            ),
            VerificationFixtureState::Cancelled => (
                VerificationSessionState::Cancelled,
                Some(terminal),
                Some(VerificationTerminalSettlement::Cancelled),
                vec![],
            ),
            VerificationFixtureState::SettledPass | VerificationFixtureState::SettledFail => {
                let conclusion = if state == VerificationFixtureState::SettledPass {
                    VerificationFindingConclusion::Pass
                } else {
                    VerificationFindingConclusion::Fail
                };
                (
                    VerificationSessionState::Settled,
                    Some(terminal),
                    Some(VerificationTerminalSettlement::Settled),
                    criterion_ids
                        .iter()
                        .map(|criterion_id| VerificationFinding {
                            role,
                            finding_ref: format!("finding-fixture-{role_id}-{}", criterion_id.0),
                            criterion_id: criterion_id.clone(),
                            conclusion,
                            source_refs: vec![fixture_source_ref(role, criterion_id)],
                            result_sequence: ExecutionSequence(2),
                            source_sequences: vec![ExecutionSequence(1)],
                            explanation: format!(
                                "{role_id} fixture finding for {}",
                                criterion_id.0
                            ),
                            candidate_ref: candidate.candidate_ref().into(),
                        })
                        .collect(),
                )
            }
            VerificationFixtureState::Missing => unreachable!("handled above"),
        };
        VerificationRoleSettlement {
            role,
            state: session_state,
            assignment: Some(assignment),
            findings,
            terminal_job_outcome,
            terminal_settlement,
        }
    }

    pub(crate) fn fixture_evidence_id(
        role: VerificationRole,
        criterion_id: &AcceptanceCriterionId,
    ) -> winwincode_domain::EvidenceId {
        winwincode_domain::EvidenceId(format!("evidence-{}-{}", role.as_str(), criterion_id.0))
    }

    pub(crate) fn fixture_source_ref(
        role: VerificationRole,
        criterion_id: &AcceptanceCriterionId,
    ) -> String {
        format!(
            "runtime_event:event-evidence-{}",
            fixture_evidence_id(role, criterion_id).0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::stage::{
        ActiveLeaseIdentity, TerminalArtifactReference, TerminalOutcomeMetadata,
        TerminalWorkerOutcome, verify_terminal_outcome,
    };
    use crate::domain::candidate::test_support::{frozen_candidate, validated_git_snapshot};
    use crate::domain::{
        Delivery, DeliveryStage, DeliveryStatus, FrozenDeliveryCandidate, SessionBinding,
        SessionBindingId, StageRun, StageRunActorType, StageRunStatus, test_fixture,
    };
    use winwincode_domain::{ArtifactId, DeliveryId, DeliveryTaskId};

    const WRITER_STAGE_ID: &str = "stage-executor-1";
    const WRITER_BINDING_ID: &str = "binding-executor-1";
    const REVIEWER_STAGE_ID: &str = "stage-reviewer-1";
    const REVIEWER_BINDING_ID: &str = "binding-reviewer-1";
    const VERIFIER_STAGE_ID: &str = "stage-verifier-1";
    const VERIFIER_BINDING_ID: &str = "binding-verifier-1";
    const ADVERSARIAL_STAGE_ID: &str = "stage-adversarial-1";
    const ADVERSARIAL_BINDING_ID: &str = "binding-adversarial-1";

    fn stage(id: &str, role: &str, started_at_millis: u64) -> StageRun {
        StageRun {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: StageRunId(id.into()),
            delivery_id: DeliveryId("delivery-main".into()),
            delivery_task_id: Some(DeliveryTaskId("delivery-task-api".into())),
            stage: if role == "executor" {
                DeliveryStage::Executing
            } else {
                DeliveryStage::Verifying
            },
            actor_type: StageRunActorType::Codex,
            role: role.into(),
            status: StageRunStatus::Succeeded,
            attempt: 1,
            started_at_millis,
            finished_at_millis: Some(started_at_millis + 10),
        }
    }

    fn binding(
        id: &str,
        stage_run_id: &str,
        identity: &str,
        bound_at_millis: u64,
    ) -> SessionBinding {
        SessionBinding {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId(id.into()),
            delivery_id: DeliveryId("delivery-main".into()),
            delivery_task_id: Some(DeliveryTaskId("delivery-task-api".into())),
            stage_run_id: StageRunId(stage_run_id.into()),
            product_session_id: ProductSessionId(format!("product-{identity}")),
            execution_job_id: ExecutionJobId(format!("job-{identity}")),
            worker_session_id: Some(WorkerSessionId(format!("worker-{identity}"))),
            codex_thread_id: Some(CodexThreadId(format!("thread-{identity}"))),
            bound_at_millis,
        }
    }

    fn fixture(
        include_adversarial: bool,
    ) -> (Delivery, FrozenDeliveryCandidate, VerificationFacts) {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.stage_runs = vec![
            stage(WRITER_STAGE_ID, "executor", 1_800_000_000_010),
            stage(REVIEWER_STAGE_ID, "reviewer", 1_800_000_000_030),
            stage(VERIFIER_STAGE_ID, "verifier", 1_800_000_000_050),
        ];
        snapshot.session_bindings = vec![
            binding(
                WRITER_BINDING_ID,
                WRITER_STAGE_ID,
                "executor",
                1_800_000_000_011,
            ),
            binding(
                REVIEWER_BINDING_ID,
                REVIEWER_STAGE_ID,
                "reviewer",
                1_800_000_000_031,
            ),
            binding(
                VERIFIER_BINDING_ID,
                VERIFIER_STAGE_ID,
                "verifier",
                1_800_000_000_051,
            ),
        ];
        let mut required_roles = vec![VerificationRole::Reviewer, VerificationRole::Verifier];
        if include_adversarial {
            snapshot.stage_runs.push(stage(
                ADVERSARIAL_STAGE_ID,
                "adversarial-verifier",
                1_800_000_000_070,
            ));
            snapshot.session_bindings.push(binding(
                ADVERSARIAL_BINDING_ID,
                ADVERSARIAL_STAGE_ID,
                "adversarial",
                1_800_000_000_071,
            ));
            required_roles.push(VerificationRole::AdversarialVerifier);
        }
        snapshot.updated_at_millis = if include_adversarial {
            1_800_000_000_080
        } else {
            1_800_000_000_060
        };
        let delivery = Delivery::try_from_snapshot(snapshot).expect("verification Delivery");
        let candidate = frozen_candidate(
            &delivery,
            &StageRunId(WRITER_STAGE_ID.into()),
            &SessionBindingId(WRITER_BINDING_ID.into()),
        );
        let mut sessions = vec![
            session_fact(
                &delivery,
                VerificationRole::Reviewer,
                REVIEWER_STAGE_ID,
                REVIEWER_BINDING_ID,
                &candidate,
            ),
            session_fact(
                &delivery,
                VerificationRole::Verifier,
                VERIFIER_STAGE_ID,
                VERIFIER_BINDING_ID,
                &candidate,
            ),
        ];
        if include_adversarial {
            sessions.push(session_fact(
                &delivery,
                VerificationRole::AdversarialVerifier,
                ADVERSARIAL_STAGE_ID,
                ADVERSARIAL_BINDING_ID,
                &candidate,
            ));
        }
        (
            delivery,
            candidate,
            VerificationFacts {
                required_roles,
                sessions,
            },
        )
    }

    fn session_fact(
        delivery: &Delivery,
        role: VerificationRole,
        stage_run_id: &str,
        session_binding_id: &str,
        candidate: &FrozenDeliveryCandidate,
    ) -> VerificationSessionFacts {
        let snapshot = validated_git_snapshot(
            delivery,
            &StageRunId(stage_run_id.into()),
            &SessionBindingId(session_binding_id.into()),
            candidate.candidate_commit_id(),
            candidate.candidate_tree_id(),
            candidate.diff_sha256(),
            candidate.changed_paths().to_vec(),
        );
        let terminal = terminal_outcome(delivery, &snapshot, TerminalOutcomeStatus::Succeeded);
        VerificationSessionFacts {
            role,
            stage_run_id: StageRunId(stage_run_id.into()),
            session_binding_id: SessionBindingId(session_binding_id.into()),
            workspace_mode: VerificationWorkspaceMode::CandidateReadOnly,
            permission_profile: VerificationPermissionProfile::CandidateReadOnlyRestricted,
            pre_candidate_snapshot: Some(snapshot.clone()),
            post_candidate_snapshot: Some(snapshot),
            accepted_job_outcome: Some(terminal),
            codex_turn_completed: true,
            mutation_records: vec![],
            findings: vec![VerificationFindingFact {
                finding_ref: format!("finding-{}", role.as_str()),
                criterion_id: AcceptanceCriterionId("criterion-required".into()),
                conclusion: VerificationFindingConclusion::Pass,
                result_sequence: ExecutionSequence(8),
                source_refs: vec![format!("runtime-event:{}/3", role.as_str())],
                source_sequences: vec![ExecutionSequence(3)],
                explanation: format!("{} checked the candidate independently", role.as_str()),
            }],
        }
    }

    fn terminal_outcome(
        delivery: &Delivery,
        snapshot: &ValidatedGitSnapshotFact,
        status: TerminalOutcomeStatus,
    ) -> AcceptedVerificationJobOutcomeFact {
        let stage_run_id = snapshot.stage_run_id();
        let final_run = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| &run.id == stage_run_id)
            .expect("final StageRun");
        let role = final_run.role.clone();
        let verified = verified_terminal_outcome(delivery, snapshot, status);
        AcceptedVerificationJobOutcomeFact::from_verified_outcome(&verified, snapshot, role)
            .expect("composite terminal outcome")
    }

    fn verified_terminal_outcome(
        delivery: &Delivery,
        snapshot: &ValidatedGitSnapshotFact,
        status: TerminalOutcomeStatus,
    ) -> VerifiedTerminalOutcome {
        let stage_run_id = snapshot.stage_run_id();
        let mut active_snapshot = delivery.clone().into_snapshot();
        let active_run = active_snapshot
            .stage_runs
            .iter_mut()
            .find(|run| &run.id == stage_run_id)
            .expect("active StageRun");
        active_run.status = StageRunStatus::Running;
        active_run.finished_at_millis = None;
        let active = Delivery::try_from_snapshot(active_snapshot).expect("active verification");
        let lease = ActiveLeaseIdentity {
            execution_job_id: snapshot.execution_job_id().clone(),
            attempt: snapshot.attempt(),
            lease_id: snapshot.lease_id().clone(),
            fencing_token: snapshot.fencing_token().clone(),
            worker_id: snapshot.worker_id().clone(),
            worker_instance_id: snapshot.worker_instance_id().clone(),
            worker_session_id: snapshot.worker_session_id().clone(),
        };
        verify_terminal_outcome(
            &active,
            &lease,
            TerminalWorkerOutcome {
                stage_run_id: stage_run_id.clone(),
                execution_job_id: lease.execution_job_id.clone(),
                attempt: lease.attempt,
                lease_id: lease.lease_id.clone(),
                fencing_token: lease.fencing_token.clone(),
                worker_id: lease.worker_id.clone(),
                worker_instance_id: lease.worker_instance_id.clone(),
                worker_session_id: lease.worker_session_id.clone(),
                status,
                metadata: TerminalOutcomeMetadata {
                    codex_thread_id: Some(snapshot.codex_thread_id().clone()),
                    finished_at_millis: snapshot.finished_at_millis(),
                    last_event_sequence: ExecutionAckSequence(
                        i64::try_from(snapshot.last_event_sequence())
                            .expect("fixture event sequence fits i64"),
                    ),
                    artifacts: vec![TerminalArtifactReference {
                        artifact_id: ArtifactId(snapshot.artifact_ref().into()),
                        digest: snapshot.artifact_digest().clone(),
                    }],
                },
            },
        )
        .expect("verified terminal outcome")
    }

    fn with_stage_status(
        delivery: &Delivery,
        stage_run_id: &str,
        status: StageRunStatus,
    ) -> Delivery {
        let mut snapshot = delivery.clone().into_snapshot();
        let run = snapshot
            .stage_runs
            .iter_mut()
            .find(|run| run.id.0 == stage_run_id)
            .expect("fixture StageRun");
        run.status = status;
        if matches!(status, StageRunStatus::Running | StageRunStatus::Waiting) {
            run.finished_at_millis = None;
        }
        Delivery::try_from_snapshot(snapshot).expect("Delivery with role status")
    }

    fn reseal_session(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
        session: &mut VerificationSessionFacts,
        outcome_status: TerminalOutcomeStatus,
    ) {
        let snapshot = validated_git_snapshot(
            delivery,
            &session.stage_run_id,
            &session.session_binding_id,
            candidate.candidate_commit_id(),
            candidate.candidate_tree_id(),
            candidate.diff_sha256(),
            candidate.changed_paths().to_vec(),
        );
        session.accepted_job_outcome = Some(terminal_outcome(delivery, &snapshot, outcome_status));
        session.pre_candidate_snapshot = Some(snapshot.clone());
        session.post_candidate_snapshot = Some(snapshot);
    }

    fn mutation_record(
        session: &VerificationSessionFacts,
        kind: VerificationMutationKind,
        succeeded: bool,
    ) -> VerificationWorkerMutationRecord {
        let outcome = session
            .accepted_job_outcome
            .as_ref()
            .expect("fixture outcome");
        VerificationWorkerMutationRecord::from_terminal_outcome(
            outcome,
            ExecutionSequence(5),
            kind,
            succeeded,
            outcome.terminal_candidate_tree_id.clone(),
        )
    }

    #[test]
    fn requires_reviewer_and_verifier() {
        let (delivery, candidate, facts) = fixture(false);
        let verification = validate_independent_verification(&delivery, &candidate, &facts)
            .expect("required verification roles");
        assert_eq!(
            verification
                .settlements()
                .iter()
                .map(VerificationRoleSettlement::role)
                .collect::<Vec<_>>(),
            [VerificationRole::Reviewer, VerificationRole::Verifier]
        );
        assert!(
            verification
                .settlements()
                .iter()
                .all(|settlement| settlement.state() == VerificationSessionState::Settled)
        );

        let mut missing_required_role = facts.clone();
        missing_required_role.required_roles = vec![VerificationRole::Verifier];
        assert!(
            validate_independent_verification(&delivery, &candidate, &missing_required_role)
                .is_err()
        );

        let mut missing_required_session = facts;
        missing_required_session.sessions.remove(0);
        let missing =
            validate_independent_verification(&delivery, &candidate, &missing_required_session)
                .expect("required role remains visible as missing");
        assert_eq!(
            missing
                .settlements()
                .iter()
                .map(|settlement| (settlement.role(), settlement.state()))
                .collect::<Vec<_>>(),
            [
                (
                    VerificationRole::Reviewer,
                    VerificationSessionState::Missing
                ),
                (
                    VerificationRole::Verifier,
                    VerificationSessionState::Settled
                ),
            ]
        );

        let (delivery, candidate, adversarial) = fixture(true);
        let verification = validate_independent_verification(&delivery, &candidate, &adversarial)
            .expect("optional adversarial verifier");
        assert_eq!(verification.settlements().len(), 3);
        assert_eq!(
            verification.settlements()[2].role(),
            VerificationRole::AdversarialVerifier
        );
    }

    #[test]
    fn missing_session_fact_still_rejects_an_ambiguous_current_assignment() {
        let (delivery, candidate, mut facts) = fixture(false);
        facts.sessions.remove(0);
        let mut ambiguous = delivery.into_snapshot();
        ambiguous.stage_runs.push(stage(
            "stage-reviewer-duplicate",
            "reviewer",
            1_800_000_000_090,
        ));
        ambiguous.session_bindings.push(binding(
            "binding-reviewer-duplicate",
            "stage-reviewer-duplicate",
            "reviewer-duplicate",
            1_800_000_000_091,
        ));
        ambiguous.updated_at_millis = 1_800_000_000_100;
        let ambiguous = Delivery::try_from_snapshot(ambiguous)
            .expect("aggregate leaves current role ambiguity to verification");

        assert!(validate_independent_verification(&ambiguous, &candidate, &facts).is_err());
    }

    #[test]
    fn missing_session_fact_still_rejects_reused_current_role_identity() {
        let (delivery, candidate, mut facts) = fixture(false);
        facts.sessions.remove(0);
        let mut reused = delivery.into_snapshot();
        reused.session_bindings[1].product_session_id =
            reused.session_bindings[0].product_session_id.clone();
        let reused = Delivery::try_from_snapshot(reused)
            .expect("aggregate leaves cross-role identity rejection to verification");

        assert!(validate_independent_verification(&reused, &candidate, &facts).is_err());
    }

    #[test]
    fn stale_session_fact_cannot_follow_a_higher_role_attempt() {
        let (delivery, candidate, facts) = fixture(false);
        let mut higher = delivery.into_snapshot();
        let mut second_attempt = stage("stage-reviewer-attempt-2", "reviewer", 1_800_000_000_090);
        second_attempt.attempt = 2;
        higher.stage_runs.push(second_attempt);
        higher.session_bindings.push(binding(
            "binding-reviewer-attempt-2",
            "stage-reviewer-attempt-2",
            "reviewer-attempt-2",
            1_800_000_000_091,
        ));
        higher.updated_at_millis = 1_800_000_000_100;
        let higher = Delivery::try_from_snapshot(higher).expect("higher role attempt");

        assert!(validate_independent_verification(&higher, &candidate, &facts).is_err());
    }

    #[test]
    fn requires_exactly_one_current_assignment_and_distinct_session_per_role() {
        let (delivery, candidate, facts) = fixture(false);
        let verification = validate_independent_verification(&delivery, &candidate, &facts)
            .expect("independent verification");
        let reviewer = verification.settlements()[0]
            .assignment()
            .expect("reviewer assignment");
        assert_eq!(reviewer.stage_run_id().0, REVIEWER_STAGE_ID);
        assert_eq!(reviewer.session_binding_id().0, REVIEWER_BINDING_ID);
        assert_eq!(reviewer.product_session_id().0, "product-reviewer");
        assert_eq!(reviewer.execution_job_id().0, "job-reviewer");
        assert_eq!(reviewer.worker_session_id().0, "worker-reviewer");
        assert_eq!(reviewer.codex_thread_id().0, "thread-reviewer");

        let mut reused_writer_identity = delivery.clone().into_snapshot();
        reused_writer_identity.session_bindings[1].product_session_id = reused_writer_identity
            .session_bindings[0]
            .product_session_id
            .clone();
        let reused_writer_identity = Delivery::try_from_snapshot(reused_writer_identity)
            .expect("aggregate permits product identity reuse for verification to reject");
        assert!(
            validate_independent_verification(&reused_writer_identity, &candidate, &facts).is_err()
        );

        let mut cross_role_identity = delivery.clone().into_snapshot();
        cross_role_identity.session_bindings[2].product_session_id = cross_role_identity
            .session_bindings[1]
            .product_session_id
            .clone();
        let cross_role_identity = Delivery::try_from_snapshot(cross_role_identity)
            .expect("aggregate permits product identity reuse for verification to reject");
        assert!(
            validate_independent_verification(&cross_role_identity, &candidate, &facts).is_err()
        );

        let mut duplicate_current_assignment = delivery.clone().into_snapshot();
        duplicate_current_assignment.stage_runs.push(stage(
            "stage-reviewer-duplicate",
            "reviewer",
            1_800_000_000_090,
        ));
        duplicate_current_assignment.session_bindings.push(binding(
            "binding-reviewer-duplicate",
            "stage-reviewer-duplicate",
            "reviewer-duplicate",
            1_800_000_000_091,
        ));
        duplicate_current_assignment.updated_at_millis = 1_800_000_000_100;
        let duplicate_current_assignment = Delivery::try_from_snapshot(
            duplicate_current_assignment,
        )
        .expect("Delivery permits concurrent same-attempt roles for verification to reject");
        assert!(
            validate_independent_verification(&duplicate_current_assignment, &candidate, &facts)
                .is_err()
        );

        let mut wrong_binding = facts;
        wrong_binding.sessions[0].session_binding_id = SessionBindingId(VERIFIER_BINDING_ID.into());
        assert!(validate_independent_verification(&delivery, &candidate, &wrong_binding).is_err());

        let (delivery, candidate, facts) = fixture(false);
        let mut duplicate_binding = delivery.clone().into_snapshot();
        duplicate_binding.session_bindings.push(binding(
            "binding-reviewer-second",
            REVIEWER_STAGE_ID,
            "reviewer-second",
            1_800_000_000_032,
        ));
        let duplicate_binding = Delivery::try_from_snapshot(duplicate_binding)
            .expect("aggregate leaves exact role-binding cardinality to verification");
        assert!(validate_independent_verification(&duplicate_binding, &candidate, &facts).is_err());

        let mut incomplete_binding = delivery.clone().into_snapshot();
        incomplete_binding.session_bindings[1].worker_session_id = None;
        incomplete_binding.session_bindings[1].codex_thread_id = None;
        let incomplete_binding = Delivery::try_from_snapshot(incomplete_binding)
            .expect("verification binding may await Worker and Codex identities");
        let mut incomplete_facts = facts;
        incomplete_facts.sessions[0].pre_candidate_snapshot = None;
        incomplete_facts.sessions[0].post_candidate_snapshot = None;
        incomplete_facts.sessions[0].accepted_job_outcome = None;
        incomplete_facts.sessions[0].findings.clear();
        let verification =
            validate_independent_verification(&incomplete_binding, &candidate, &incomplete_facts)
                .expect("partial current assignment remains incomplete");
        assert_eq!(
            verification.settlements()[0].state(),
            VerificationSessionState::Incomplete
        );
    }

    #[test]
    fn running_role_keeps_its_exact_read_only_checkout_assignment() {
        let (delivery, candidate, mut facts) = fixture(false);
        let running = with_stage_status(&delivery, REVIEWER_STAGE_ID, StageRunStatus::Running);
        let reviewer = &mut facts.sessions[0];
        reviewer.accepted_job_outcome = None;
        reviewer.post_candidate_snapshot = None;
        reviewer.findings.clear();
        reviewer.codex_turn_completed = false;

        let verification = validate_independent_verification(&running, &candidate, &facts)
            .expect("running verification keeps its accepted candidate checkout");
        let reviewer = &verification.settlements()[0];
        assert_eq!(reviewer.state(), VerificationSessionState::Running);
        assert!(reviewer.assignment().is_some());
        assert!(reviewer.terminal_job_outcome().is_none());
        assert!(reviewer.terminal_settlement().is_none());
    }

    #[test]
    fn running_role_rejects_a_checkout_from_another_stage_and_session() {
        let (delivery, candidate, mut facts) = fixture(false);
        let running = with_stage_status(&delivery, REVIEWER_STAGE_ID, StageRunStatus::Running);

        let foreign_checkout = validated_git_snapshot(
            &delivery,
            &StageRunId(VERIFIER_STAGE_ID.into()),
            &SessionBindingId(REVIEWER_BINDING_ID.into()),
            candidate.candidate_commit_id(),
            candidate.candidate_tree_id(),
            candidate.diff_sha256(),
            candidate.changed_paths().to_vec(),
        );

        let reviewer = &mut facts.sessions[0];
        reviewer.pre_candidate_snapshot = Some(foreign_checkout);
        reviewer.post_candidate_snapshot = None;
        reviewer.accepted_job_outcome = None;
        reviewer.findings.clear();

        assert!(validate_independent_verification(&running, &candidate, &facts).is_err());
    }

    #[test]
    fn verification_roles_require_candidate_read_only_policy() {
        let (delivery, candidate, facts) = fixture(false);
        validate_independent_verification(&delivery, &candidate, &facts)
            .expect("candidate read-only verification");

        for mode in [
            VerificationWorkspaceMode::CandidateWrite,
            VerificationWorkspaceMode::SourceReadOnly,
        ] {
            let mut wrong_mode = facts.clone();
            wrong_mode.sessions[0].workspace_mode = mode;
            assert!(validate_independent_verification(&delivery, &candidate, &wrong_mode).is_err());
        }

        let mut unrestricted = facts.clone();
        unrestricted.sessions[1].permission_profile = VerificationPermissionProfile::Unrestricted;
        assert!(validate_independent_verification(&delivery, &candidate, &unrestricted).is_err());

        let (delivery, candidate, mut adversarial) = fixture(true);
        adversarial.sessions[2].workspace_mode = VerificationWorkspaceMode::CandidateWrite;
        assert!(validate_independent_verification(&delivery, &candidate, &adversarial).is_err());
    }

    #[test]
    fn rejects_verification_when_sealed_pre_and_post_candidate_snapshots_differ() {
        let (delivery, candidate, facts) = fixture(false);

        let mut changed_tree = facts.clone();
        changed_tree.sessions[0].post_candidate_snapshot = Some(validated_git_snapshot(
            &delivery,
            &StageRunId(REVIEWER_STAGE_ID.into()),
            &SessionBindingId(REVIEWER_BINDING_ID.into()),
            candidate.candidate_commit_id(),
            "5555555555555555555555555555555555555555",
            candidate.diff_sha256(),
            candidate.changed_paths().to_vec(),
        ));
        assert!(validate_independent_verification(&delivery, &candidate, &changed_tree).is_err());

        let mut changed_diff = facts.clone();
        changed_diff.sessions[1].post_candidate_snapshot = Some(validated_git_snapshot(
            &delivery,
            &StageRunId(VERIFIER_STAGE_ID.into()),
            &SessionBindingId(VERIFIER_BINDING_ID.into()),
            candidate.candidate_commit_id(),
            candidate.candidate_tree_id(),
            &"b".repeat(64),
            candidate.changed_paths().to_vec(),
        ));
        assert!(validate_independent_verification(&delivery, &candidate, &changed_diff).is_err());

        for kind in [
            VerificationMutationKind::FileWrite,
            VerificationMutationKind::PatchApply,
        ] {
            let mut successful_write = facts.clone();
            let record = mutation_record(&successful_write.sessions[1], kind, true);
            successful_write.sessions[1].mutation_records.push(record);
            assert!(
                validate_independent_verification(&delivery, &candidate, &successful_write)
                    .is_err()
            );

            let mut rejected_write = facts.clone();
            let record = mutation_record(&rejected_write.sessions[0], kind, false);
            rejected_write.sessions[0].mutation_records.push(record);
            validate_independent_verification(&delivery, &candidate, &rejected_write)
                .expect("failed mutation attempt did not change the candidate");
        }

        let mut stale_worker_record = facts.clone();
        let mut record = mutation_record(
            &stale_worker_record.sessions[0],
            VerificationMutationKind::FileWrite,
            false,
        );
        record.fencing_token = FencingToken("2".into());
        stale_worker_record.sessions[0]
            .mutation_records
            .push(record);
        assert!(
            validate_independent_verification(&delivery, &candidate, &stale_worker_record).is_err()
        );

        let (delivery, candidate, mut adversarial) = fixture(true);
        let record = mutation_record(
            &adversarial.sessions[2],
            VerificationMutationKind::PatchApply,
            true,
        );
        adversarial.sessions[2].mutation_records.push(record);
        assert!(validate_independent_verification(&delivery, &candidate, &adversarial).is_err());
    }

    #[test]
    fn unchanged_candidate_allows_ordered_pre_and_post_snapshot_observations() {
        let (delivery, candidate, mut facts) = fixture(false);
        let mut settled_snapshot = delivery.clone().into_snapshot();
        settled_snapshot.stage_runs[1].finished_at_millis = Some(1_800_000_000_041);
        let settled_delivery = Delivery::try_from_snapshot(settled_snapshot)
            .expect("later terminal observation remains canonical");
        let post = validated_git_snapshot(
            &settled_delivery,
            &StageRunId(REVIEWER_STAGE_ID.into()),
            &SessionBindingId(REVIEWER_BINDING_ID.into()),
            candidate.candidate_commit_id(),
            candidate.candidate_tree_id(),
            candidate.diff_sha256(),
            candidate.changed_paths().to_vec(),
        );
        let reviewer = &mut facts.sessions[0];
        reviewer.accepted_job_outcome = Some(terminal_outcome(
            &settled_delivery,
            &post,
            TerminalOutcomeStatus::Succeeded,
        ));
        reviewer.post_candidate_snapshot = Some(post);

        validate_independent_verification(&settled_delivery, &candidate, &facts)
            .expect("unchanged Git facts survive ordered pre/post observations");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn requires_succeeded_stage_and_sealed_matching_terminal_outcome() {
        let (delivery, candidate, facts) = fixture(false);
        let verification = validate_independent_verification(&delivery, &candidate, &facts)
            .expect("accepted terminal outcomes");
        let reviewer = &verification.settlements()[0];
        let outcome = reviewer
            .terminal_job_outcome()
            .expect("validated terminal job outcome");
        assert_eq!(outcome.product_session_id().0, "product-reviewer");
        assert_eq!(outcome.stage_run_id().0, REVIEWER_STAGE_ID);
        assert_eq!(outcome.role_id(), "reviewer");
        assert_eq!(outcome.execution_job_id().0, "job-reviewer");
        assert_eq!(outcome.attempt(), 1);
        assert_eq!(outcome.lease_id().0, "lease-stage-reviewer-1");
        assert_eq!(outcome.fencing_token().0, "1");
        assert_eq!(outcome.worker_id().0, "worker-candidate");
        assert_eq!(outcome.worker_instance_id().0, "worker-instance-candidate");
        assert_eq!(outcome.worker_session_id().0, "worker-reviewer");
        assert_eq!(outcome.codex_thread_id().0, "thread-reviewer");
        assert_eq!(outcome.last_event_sequence().0, 12);
        assert_eq!(outcome.finished_at_millis(), 1_800_000_000_040);
        assert_eq!(
            outcome.terminal_candidate_tree_id(),
            candidate.candidate_tree_id()
        );
        let finding = &reviewer.findings()[0];
        assert_eq!(finding.result_sequence().0, 8);
        assert_eq!(finding.source_sequences()[0].0, 3);
        assert_eq!(
            reviewer.terminal_settlement(),
            Some(VerificationTerminalSettlement::Settled)
        );

        let mut mismatched_status = facts.clone();
        let pre_snapshot = mismatched_status.sessions[0]
            .pre_candidate_snapshot
            .as_ref()
            .expect("pre snapshot")
            .clone();
        mismatched_status.sessions[0].accepted_job_outcome = Some(terminal_outcome(
            &delivery,
            &pre_snapshot,
            TerminalOutcomeStatus::Failed,
        ));
        assert!(
            validate_independent_verification(&delivery, &candidate, &mismatched_status).is_err()
        );

        let failed_delivery =
            with_stage_status(&delivery, REVIEWER_STAGE_ID, StageRunStatus::Failed);
        let mut failed_facts = facts.clone();
        reseal_session(
            &failed_delivery,
            &candidate,
            &mut failed_facts.sessions[0],
            TerminalOutcomeStatus::Failed,
        );
        let failed = validate_independent_verification(&failed_delivery, &candidate, &failed_facts)
            .expect("failed terminal role remains visible");
        assert_eq!(
            failed.settlements()[0].state(),
            VerificationSessionState::Failed
        );
        assert_eq!(
            failed.settlements()[0].terminal_settlement(),
            Some(VerificationTerminalSettlement::Failed)
        );

        let mut infrastructure_facts = facts.clone();
        reseal_session(
            &failed_delivery,
            &candidate,
            &mut infrastructure_facts.sessions[0],
            TerminalOutcomeStatus::InfrastructureError,
        );
        let infrastructure =
            validate_independent_verification(&failed_delivery, &candidate, &infrastructure_facts)
                .expect("infrastructure terminal role remains fail closed");
        assert_eq!(
            infrastructure.settlements()[0].state(),
            VerificationSessionState::Failed
        );
        assert_eq!(
            infrastructure.settlements()[0].terminal_settlement(),
            Some(VerificationTerminalSettlement::InfrastructureError)
        );

        let cancelled_delivery =
            with_stage_status(&delivery, REVIEWER_STAGE_ID, StageRunStatus::Cancelled);
        let mut cancelled_facts = facts.clone();
        reseal_session(
            &cancelled_delivery,
            &candidate,
            &mut cancelled_facts.sessions[0],
            TerminalOutcomeStatus::Cancelled,
        );
        let cancelled =
            validate_independent_verification(&cancelled_delivery, &candidate, &cancelled_facts)
                .expect("cancelled terminal role remains visible");
        assert_eq!(
            cancelled.settlements()[0].state(),
            VerificationSessionState::Cancelled
        );

        let mut turn_only = facts.clone();
        turn_only.sessions[0].accepted_job_outcome = None;
        turn_only.sessions[0].pre_candidate_snapshot = None;
        turn_only.sessions[0].post_candidate_snapshot = None;
        assert!(turn_only.sessions[0].codex_turn_completed);
        let verification = validate_independent_verification(&delivery, &candidate, &turn_only)
            .expect("turn completion alone is incomplete");
        assert_eq!(
            verification.settlements()[0].state(),
            VerificationSessionState::Incomplete
        );

        let mut running_snapshot = delivery.clone().into_snapshot();
        running_snapshot.stage_runs[1].status = StageRunStatus::Running;
        running_snapshot.stage_runs[1].finished_at_millis = None;
        let running = Delivery::try_from_snapshot(running_snapshot).expect("running reviewer");
        let mut running_facts = facts.clone();
        running_facts.sessions[0].accepted_job_outcome = None;
        running_facts.sessions[0].pre_candidate_snapshot = None;
        running_facts.sessions[0].post_candidate_snapshot = None;
        let verification = validate_independent_verification(&running, &candidate, &running_facts)
            .expect("running StageRun remains running");
        assert_eq!(
            verification.settlements()[0].state(),
            VerificationSessionState::Running
        );

        let mut wrong_job = facts.clone();
        wrong_job.sessions[0]
            .accepted_job_outcome
            .as_mut()
            .expect("outcome")
            .execution_job_id = ExecutionJobId("job-foreign".into());
        assert!(validate_independent_verification(&delivery, &candidate, &wrong_job).is_err());

        let mut changed_tree = facts.clone();
        changed_tree.sessions[0]
            .accepted_job_outcome
            .as_mut()
            .expect("outcome")
            .terminal_candidate_tree_id = "5555555555555555555555555555555555555555".into();
        assert!(validate_independent_verification(&delivery, &candidate, &changed_tree).is_err());
    }

    #[test]
    fn accepted_terminal_outcome_rejects_snapshot_metadata_from_another_outcome() {
        let (delivery, candidate, facts) = fixture(false);
        let accepted_snapshot = facts.sessions[0]
            .pre_candidate_snapshot
            .as_ref()
            .expect("accepted reviewer snapshot");
        let verified = verified_terminal_outcome(
            &delivery,
            accepted_snapshot,
            TerminalOutcomeStatus::Succeeded,
        );

        let mut substituted_delivery = delivery.clone().into_snapshot();
        substituted_delivery.stage_runs[1].finished_at_millis = Some(1_800_000_000_041);
        let substituted_delivery = Delivery::try_from_snapshot(substituted_delivery)
            .expect("substituted terminal time remains a valid Delivery snapshot");
        let substituted_snapshot = validated_git_snapshot(
            &substituted_delivery,
            &StageRunId(REVIEWER_STAGE_ID.into()),
            &SessionBindingId(REVIEWER_BINDING_ID.into()),
            candidate.candidate_commit_id(),
            candidate.candidate_tree_id(),
            candidate.diff_sha256(),
            candidate.changed_paths().to_vec(),
        );

        assert!(
            AcceptedVerificationJobOutcomeFact::from_verified_outcome(
                &verified,
                &substituted_snapshot,
                "reviewer",
            )
            .is_err()
        );
    }

    #[test]
    fn checkout_must_match_candidate_repository_and_commit() {
        let (delivery, candidate, facts) = fixture(false);
        let verification = validate_independent_verification(&delivery, &candidate, &facts)
            .expect("exact candidate checkout");
        let reviewer = verification.settlements()[0]
            .assignment()
            .expect("reviewer assignment");
        assert_eq!(reviewer.repository(), candidate.repository());
        assert_eq!(
            reviewer.checkout_revision(),
            candidate.candidate_commit_id()
        );

        let mut wrong_revision = facts.clone();
        let wrong_checkout = validated_git_snapshot(
            &delivery,
            &StageRunId(REVIEWER_STAGE_ID.into()),
            &SessionBindingId(REVIEWER_BINDING_ID.into()),
            candidate.base_commit_id(),
            candidate.candidate_tree_id(),
            candidate.diff_sha256(),
            candidate.changed_paths().to_vec(),
        );
        wrong_revision.sessions[0].pre_candidate_snapshot = Some(wrong_checkout.clone());
        wrong_revision.sessions[0].post_candidate_snapshot = Some(wrong_checkout);
        assert!(validate_independent_verification(&delivery, &candidate, &wrong_revision).is_err());

        let mut foreign_snapshot = delivery.clone().into_snapshot();
        foreign_snapshot.spec.repository.locator = "file:///foreign".into();
        let foreign_delivery =
            Delivery::try_from_snapshot(foreign_snapshot).expect("foreign repository Delivery");
        let foreign_checkout = validated_git_snapshot(
            &foreign_delivery,
            &StageRunId(VERIFIER_STAGE_ID.into()),
            &SessionBindingId(VERIFIER_BINDING_ID.into()),
            candidate.candidate_commit_id(),
            candidate.candidate_tree_id(),
            candidate.diff_sha256(),
            candidate.changed_paths().to_vec(),
        );
        let mut wrong_repository = facts;
        wrong_repository.sessions[1].pre_candidate_snapshot = Some(foreign_checkout.clone());
        wrong_repository.sessions[1].post_candidate_snapshot = Some(foreign_checkout);
        assert!(
            validate_independent_verification(&delivery, &candidate, &wrong_repository).is_err()
        );
    }

    #[test]
    fn findings_must_be_bounded_by_the_accepted_terminal_sequence() {
        let (delivery, candidate, facts) = fixture(false);

        let mut result_after_terminal = facts.clone();
        result_after_terminal.sessions[0].findings[0].result_sequence = ExecutionSequence(13);
        assert!(
            validate_independent_verification(&delivery, &candidate, &result_after_terminal)
                .is_err()
        );

        let mut source_after_result = facts;
        source_after_result.sessions[1].findings[0].source_sequences = vec![ExecutionSequence(8)];
        assert!(
            validate_independent_verification(&delivery, &candidate, &source_after_result).is_err()
        );
    }

    #[test]
    fn test_support_builds_only_sealed_role_projections() {
        use super::test_support::{VerificationFixtureState, independent_verification};

        let (delivery, candidate, _) = fixture(false);
        let states = [
            VerificationFixtureState::Missing,
            VerificationFixtureState::Running,
            VerificationFixtureState::Incomplete,
            VerificationFixtureState::Failed,
            VerificationFixtureState::InfrastructureFailed,
            VerificationFixtureState::Cancelled,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledFail,
        ];
        for state in states {
            let verification = independent_verification(
                &delivery,
                &candidate,
                state,
                VerificationFixtureState::SettledPass,
            );
            assert_eq!(verification.candidate_ref(), candidate.candidate_ref());
            assert_eq!(verification.settlements().len(), 2);
        }
    }
}
