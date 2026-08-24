// SPDX-License-Identifier: Apache-2.0

//! Exact Evidence source resolution for the current frozen candidate.

use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};
use winwincode_domain::{
    CodexThreadId, DeliveryId, EvidenceId, ExecutionEventId, ExecutionJobId, FencingToken, LeaseId,
    ProductSessionId, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

use super::verification::AcceptedVerificationJobOutcomeFact;
use super::{
    CandidatePathState, Delivery, DeliverySpecId, DeliveryStage, DeliveryValidationError,
    FrozenDeliveryCandidate, MAX_REFERENCE_LENGTH, MAX_SAFE_INTEGER, RepositoryRef, SessionBinding,
    SessionBindingId, StageRun, StageRunActorType, assert_frozen_candidate_current, bounded_text,
    portable_identifier, positive, safe_non_negative, schema_version,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceRefType {
    #[serde(rename = "test")]
    Test,
    #[serde(rename = "command")]
    Command,
    #[serde(rename = "diff")]
    Diff,
    #[serde(rename = "file")]
    File,
    #[serde(rename = "commit")]
    Commit,
    #[serde(rename = "pull_request")]
    PullRequest,
    #[serde(rename = "runtime_event")]
    RuntimeEvent,
    #[serde(rename = "review_finding")]
    ReviewFinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceRef {
    pub schema_version: u8,
    pub id: EvidenceId,
    pub delivery_id: DeliveryId,
    pub delivery_spec_id: DeliverySpecId,
    pub delivery_spec_revision: u64,
    pub stage_run_id: StageRunId,
    pub session_binding_id: SessionBindingId,
    pub candidate_ref: String,
    #[serde(rename = "type")]
    pub evidence_type: EvidenceRefType,
    pub source_ref: String,
    pub created_at_millis: u64,
}

/// Outcome classified from one already persisted source fact.
///
/// This value is sealed inside [`ResolvedDeliveryEvidence`]. API callers do not
/// submit it to verdict computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "verdict consumes all classified outcomes after branch integration"
    )
)]
pub(crate) enum VerifiedEvidenceOutcome {
    Observed,
    Succeeded,
    Failed,
    TimedOut,
    PolicyDenied,
    InfrastructureFailed,
    Cancelled,
}

/// Sealed checkout attestation rebuilt by the Worker/Artifact adapter.
///
/// Fields are private and this type has no public constructor or deserializer.
/// Until its owning adapter lands, only this module's tests can construct it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedCheckoutAttestationFact {
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    stage_run_id: StageRunId,
    role_id: String,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    repository: RepositoryRef,
    checkout_commit_id: String,
    checkout_tree_id: String,
}

/// One source rebuilt from the accepted append-only runtime ledger.
///
/// This sealed fact stores identity, ordering, and direct outcome only. It never
/// copies a log body, command output, model response, or complete `RuntimeEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AcceptedRuntimeSourceFact {
    source_event_id: ExecutionEventId,
    evidence_type: EvidenceRefType,
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    stage_run_id: StageRunId,
    role_id: String,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    source_sequence: u64,
    candidate_ref: String,
    occurred_at_millis: u64,
    outcome: VerifiedEvidenceOutcome,
}

/// Fenced execution identity shared by the terminal outcome, source ledger,
/// and checkout attestation. Comparing this value in one place prevents one
/// boundary from accidentally omitting an identity component.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FencedExecutionIdentity {
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    stage_run_id: StageRunId,
    role_id: String,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
}

/// Selects one bounded source. Runtime identity and Git references are derived
/// by the resolver rather than supplied as canonical Evidence fields.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Phase 3/4 adapter gate keeps production construction closed"
    )
)]
#[derive(Debug)]
pub(crate) enum EvidenceSource<'facts> {
    Runtime {
        evidence_type: EvidenceRefType,
        source_event_id: ExecutionEventId,
        accepted_sources: &'facts [AcceptedRuntimeSourceFact],
        terminal: &'facts AcceptedVerificationJobOutcomeFact,
        checkout: &'facts ValidatedCheckoutAttestationFact,
    },
    CandidateCommit,
    CandidateDiff,
    CandidateFile {
        path: String,
    },
}

#[derive(Debug)]
pub(crate) struct ResolveDeliveryEvidenceInput<'facts> {
    pub evidence_id: EvidenceId,
    pub stage_run_id: StageRunId,
    pub session_binding_id: SessionBindingId,
    pub source: EvidenceSource<'facts>,
    pub created_at_millis: u64,
}

#[derive(Debug)]
struct ResolvedEvidenceSource {
    evidence_type: EvidenceRefType,
    source_ref: String,
    outcome: VerifiedEvidenceOutcome,
    identity: VerifiedEvidenceSourceIdentity,
}

/// Exact identity that was checked before an `EvidenceRef` became canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerifiedEvidenceSourceIdentity {
    Runtime(Box<VerifiedRuntimeEvidenceSourceIdentity>),
    CandidateCommit {
        candidate_ref: String,
        candidate_commit_id: String,
    },
    CandidateDiff {
        candidate_ref: String,
        diff_sha256: String,
    },
    CandidateFile {
        candidate_ref: String,
        candidate_tree_id: String,
        path: String,
        object_id: String,
    },
}

/// Read-only runtime provenance retained beside canonical bounded Evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedRuntimeEvidenceSourceIdentity {
    execution: FencedExecutionIdentity,
    source_sequence: u64,
    terminal_last_event_sequence: u64,
    candidate_ref: String,
    checkout_commit_id: String,
    checkout_tree_id: String,
}

/// Canonical Evidence plus source facts that were checked by this module.
///
/// Fields stay private and this type has no deserializer or public constructor.
/// Verdict code must consume this value instead of caller-provided Evidence or
/// caller-provided outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDeliveryEvidence {
    evidence: EvidenceRef,
    outcome: VerifiedEvidenceOutcome,
    source_identity: VerifiedEvidenceSourceIdentity,
}

impl ResolvedDeliveryEvidence {
    pub fn evidence(&self) -> &EvidenceRef {
        &self.evidence
    }

    pub(crate) const fn outcome(&self) -> VerifiedEvidenceOutcome {
        self.outcome
    }

    /// Confirms that this Evidence is the exact accepted runtime position a
    /// structured verification finding cited, under the same terminal lease.
    pub(crate) fn matches_finding_source(
        &self,
        terminal: &AcceptedVerificationJobOutcomeFact,
        source_sequence: u64,
    ) -> bool {
        match &self.source_identity {
            VerifiedEvidenceSourceIdentity::Runtime(identity) => {
                identity.execution == terminal_identity(terminal)
                    && identity.source_sequence == source_sequence
                    && identity.terminal_last_event_sequence == accepted_terminal_sequence(terminal)
            }
            VerifiedEvidenceSourceIdentity::CandidateCommit { .. }
            | VerifiedEvidenceSourceIdentity::CandidateDiff { .. }
            | VerifiedEvidenceSourceIdentity::CandidateFile { .. } => false,
        }
    }

    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "verdict consumes the verified source identity after branch integration"
        )
    )]
    pub(crate) fn source_identity(&self) -> &VerifiedEvidenceSourceIdentity {
        &self.source_identity
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvidenceResolutionErrorCode {
    InvalidEvidence,
    CandidateStale,
    StageMismatch,
    SessionMismatch,
    SourceMissing,
    SourceAmbiguous,
    TypeMismatch,
    CandidateMismatch,
    SourceTimeMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvidenceResolutionError {
    code: EvidenceResolutionErrorCode,
    message: String,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Phase 3/4 adapter maps these stable errors after integration"
    )
)]
impl EvidenceResolutionError {
    pub(crate) fn code(&self) -> EvidenceResolutionErrorCode {
        self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for EvidenceResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for EvidenceResolutionError {}

fn resolution_error(
    code: EvidenceResolutionErrorCode,
    message: impl Into<String>,
) -> EvidenceResolutionError {
    EvidenceResolutionError {
        code,
        message: message.into(),
    }
}

/// Resolves one current `EvidenceRef` without starting runtime work.
///
/// # Errors
///
/// Rejects a stale candidate, foreign `StageRun` or `SessionBinding`, missing or
/// ambiguous source facts, identity/type/candidate drift, and sources that do
/// not strictly precede the resulting Evidence.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Phase 3/4 adapter gate keeps production construction closed"
    )
)]
pub(crate) fn resolve_delivery_evidence(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
    input: ResolveDeliveryEvidenceInput<'_>,
) -> Result<ResolvedDeliveryEvidence, EvidenceResolutionError> {
    assert_frozen_candidate_current(delivery, candidate).map_err(|error| {
        resolution_error(
            EvidenceResolutionErrorCode::CandidateStale,
            format!("frozen candidate is not current: {error}"),
        )
    })?;
    let (stage_run, binding, producer) = evidence_stage_and_binding(delivery, candidate, &input)?;

    let source = match &input.source {
        EvidenceSource::Runtime {
            evidence_type,
            source_event_id,
            accepted_sources,
            terminal,
            checkout,
        } => resolve_runtime_source(
            candidate,
            stage_run,
            binding,
            *evidence_type,
            source_event_id,
            accepted_sources,
            terminal,
            checkout,
            input.created_at_millis,
        )?,
        direct => resolve_direct_candidate_source(
            candidate,
            stage_run,
            binding,
            producer,
            direct,
            input.created_at_millis,
        )?,
    };

    let evidence = EvidenceRef {
        schema_version: super::DELIVERY_SCHEMA_VERSION,
        id: input.evidence_id,
        delivery_id: delivery.id().clone(),
        delivery_spec_id: delivery.snapshot().spec.id.clone(),
        delivery_spec_revision: delivery.snapshot().spec.revision,
        stage_run_id: stage_run.id.clone(),
        session_binding_id: binding.id.clone(),
        candidate_ref: candidate.candidate_ref().into(),
        evidence_type: source.evidence_type,
        source_ref: source.source_ref,
        created_at_millis: input.created_at_millis,
    };
    validate(&evidence, "evidence").map_err(|error| {
        resolution_error(
            EvidenceResolutionErrorCode::InvalidEvidence,
            format!("resolved EvidenceRef is invalid: {error}"),
        )
    })?;

    Ok(ResolvedDeliveryEvidence {
        evidence,
        outcome: source.outcome,
        source_identity: source.identity,
    })
}

fn resolve_direct_candidate_source(
    candidate: &FrozenDeliveryCandidate,
    stage_run: &StageRun,
    binding: &SessionBinding,
    producer: &StageRun,
    source: &EvidenceSource<'_>,
    evidence_created_at_millis: u64,
) -> Result<ResolvedEvidenceSource, EvidenceResolutionError> {
    assert_direct_candidate_producer(
        candidate,
        stage_run,
        binding,
        producer,
        evidence_created_at_millis,
    )?;
    match source {
        EvidenceSource::CandidateCommit => Ok(ResolvedEvidenceSource {
            evidence_type: EvidenceRefType::Commit,
            source_ref: format!("git_commit:{}", candidate.candidate_commit_id()),
            outcome: VerifiedEvidenceOutcome::Observed,
            identity: VerifiedEvidenceSourceIdentity::CandidateCommit {
                candidate_ref: candidate.candidate_ref().into(),
                candidate_commit_id: candidate.candidate_commit_id().into(),
            },
        }),
        EvidenceSource::CandidateDiff => Ok(ResolvedEvidenceSource {
            evidence_type: EvidenceRefType::Diff,
            source_ref: format!("git_diff:sha256:{}", candidate.diff_sha256()),
            outcome: VerifiedEvidenceOutcome::Observed,
            identity: VerifiedEvidenceSourceIdentity::CandidateDiff {
                candidate_ref: candidate.candidate_ref().into(),
                diff_sha256: candidate.diff_sha256().into(),
            },
        }),
        EvidenceSource::CandidateFile { path } => resolve_direct_candidate_file(candidate, path),
        EvidenceSource::Runtime { .. } => unreachable!("runtime source resolved separately"),
    }
}

fn resolve_direct_candidate_file(
    candidate: &FrozenDeliveryCandidate,
    path: &str,
) -> Result<ResolvedEvidenceSource, EvidenceResolutionError> {
    let (fact, object_id) = candidate
        .changed_paths()
        .iter()
        .find(|fact| fact.path == path)
        .filter(|fact| fact.state == CandidatePathState::Present)
        .and_then(|fact| fact.object_id.as_deref().map(|object_id| (fact, object_id)))
        .ok_or_else(|| {
            resolution_error(
                EvidenceResolutionErrorCode::SourceMissing,
                "candidate file is absent from the frozen changed-path facts",
            )
        })?;
    Ok(ResolvedEvidenceSource {
        evidence_type: EvidenceRefType::File,
        source_ref: format!(
            "git_file:{}:{}@{}",
            candidate.candidate_tree_id(),
            encode_uri_component(&fact.path),
            object_id
        ),
        outcome: VerifiedEvidenceOutcome::Observed,
        identity: VerifiedEvidenceSourceIdentity::CandidateFile {
            candidate_ref: candidate.candidate_ref().into(),
            candidate_tree_id: candidate.candidate_tree_id().into(),
            path: fact.path.clone(),
            object_id: object_id.into(),
        },
    })
}

fn assert_direct_candidate_producer(
    candidate: &FrozenDeliveryCandidate,
    stage_run: &StageRun,
    binding: &SessionBinding,
    producer: &StageRun,
    evidence_created_at_millis: u64,
) -> Result<(), EvidenceResolutionError> {
    if stage_run.id != producer.id
        || stage_run.id != *candidate.producer_stage_run_id()
        || binding.id != *candidate.producer_session_binding_id()
    {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SessionMismatch,
            "direct candidate evidence must retain its producer StageRun and SessionBinding",
        ));
    }
    if producer
        .finished_at_millis
        .is_none_or(|finished| evidence_created_at_millis < finished)
    {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SourceTimeMismatch,
            "direct candidate evidence cannot predate its producer result",
        ));
    }
    Ok(())
}

fn encode_uri_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn evidence_stage_and_binding<'delivery>(
    delivery: &'delivery Delivery,
    candidate: &FrozenDeliveryCandidate,
    input: &ResolveDeliveryEvidenceInput<'_>,
) -> Result<
    (
        &'delivery StageRun,
        &'delivery SessionBinding,
        &'delivery StageRun,
    ),
    EvidenceResolutionError,
> {
    let stage_run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == input.stage_run_id && run.actor_type == StageRunActorType::Codex)
        .ok_or_else(|| {
            resolution_error(
                EvidenceResolutionErrorCode::StageMismatch,
                "evidence StageRun is missing or is not owned by Codex",
            )
        })?;
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.id == input.session_binding_id)
        .ok_or_else(|| {
            resolution_error(
                EvidenceResolutionErrorCode::SessionMismatch,
                "evidence SessionBinding is missing",
            )
        })?;
    if binding.delivery_id != *delivery.id()
        || binding.stage_run_id != stage_run.id
        || binding.delivery_task_id != stage_run.delivery_task_id
        || binding.worker_session_id.is_none()
        || binding.codex_thread_id.is_none()
        || binding.bound_at_millis < stage_run.started_at_millis
        || input.created_at_millis < binding.bound_at_millis
        || input.created_at_millis < stage_run.started_at_millis
    {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SessionMismatch,
            "evidence does not match one complete current SessionBinding",
        ));
    }
    let producer = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == *candidate.producer_stage_run_id())
        .ok_or_else(|| {
            resolution_error(
                EvidenceResolutionErrorCode::CandidateStale,
                "candidate producer StageRun is missing",
            )
        })?;
    if stage_run.id != producer.id {
        let producer_finished = producer.finished_at_millis.ok_or_else(|| {
            resolution_error(
                EvidenceResolutionErrorCode::CandidateStale,
                "candidate producer StageRun did not finish",
            )
        })?;
        if stage_run.stage != DeliveryStage::Verifying
            || stage_run.delivery_task_id != producer.delivery_task_id
            || stage_run.started_at_millis < producer_finished
            || binding.bound_at_millis < producer_finished
        {
            return Err(resolution_error(
                EvidenceResolutionErrorCode::CandidateMismatch,
                "evidence StageRun does not consume the current candidate task scope",
            ));
        }
    }
    Ok((stage_run, binding, producer))
}

#[allow(clippy::too_many_arguments)]
fn resolve_runtime_source(
    candidate: &FrozenDeliveryCandidate,
    stage_run: &StageRun,
    binding: &SessionBinding,
    evidence_type: EvidenceRefType,
    source_event_id: &ExecutionEventId,
    accepted_sources: &[AcceptedRuntimeSourceFact],
    terminal: &AcceptedVerificationJobOutcomeFact,
    checkout: &ValidatedCheckoutAttestationFact,
    evidence_created_at_millis: u64,
) -> Result<ResolvedEvidenceSource, EvidenceResolutionError> {
    if evidence_type == EvidenceRefType::PullRequest {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::InvalidEvidence,
            "runtime evidence type is unsupported",
        ));
    }
    portable_identifier(&source_event_id.0, "evidence.sourceEventId").map_err(|error| {
        resolution_error(
            EvidenceResolutionErrorCode::InvalidEvidence,
            format!("runtime source position is invalid: {error}"),
        )
    })?;
    let expected_identity = validate_runtime_terminal(
        candidate,
        stage_run,
        binding,
        terminal,
        evidence_created_at_millis,
    )?;
    validate_checkout_attestation(candidate, checkout, &expected_identity)?;
    let matches: Vec<_> = accepted_sources
        .iter()
        .filter(|fact| fact.source_event_id == *source_event_id)
        .collect();
    if matches.is_empty() {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SourceMissing,
            "accepted runtime evidence source position is missing",
        ));
    }
    if matches.len() != 1 {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SourceAmbiguous,
            "accepted runtime evidence source position occurs more than once",
        ));
    }
    let fact = matches[0];
    if runtime_source_identity(fact) != expected_identity {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SessionMismatch,
            "runtime source belongs to another product, job, worker, thread, stage, or role",
        ));
    }
    if fact.evidence_type != evidence_type {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::TypeMismatch,
            "runtime source type does not match the requested Evidence type",
        ));
    }
    if fact.candidate_ref != candidate.candidate_ref() {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::CandidateMismatch,
            "runtime source belongs to another frozen candidate",
        ));
    }
    validate_runtime_source_position(
        fact,
        stage_run,
        binding,
        terminal,
        evidence_created_at_millis,
    )?;
    let source_ref = format!("runtime_event:{}", source_event_id.0);
    bounded_text(&source_ref, "evidence.sourceRef", MAX_REFERENCE_LENGTH).map_err(|error| {
        resolution_error(
            EvidenceResolutionErrorCode::InvalidEvidence,
            format!("runtime source reference is invalid: {error}"),
        )
    })?;
    Ok(ResolvedEvidenceSource {
        evidence_type,
        source_ref,
        outcome: fact.outcome,
        identity: VerifiedEvidenceSourceIdentity::Runtime(Box::new(
            VerifiedRuntimeEvidenceSourceIdentity {
                execution: expected_identity,
                source_sequence: fact.source_sequence,
                terminal_last_event_sequence: accepted_terminal_sequence(terminal),
                candidate_ref: fact.candidate_ref.clone(),
                checkout_commit_id: checkout.checkout_commit_id.clone(),
                checkout_tree_id: checkout.checkout_tree_id.clone(),
            },
        )),
    })
}

fn validate_runtime_terminal(
    candidate: &FrozenDeliveryCandidate,
    stage_run: &StageRun,
    binding: &SessionBinding,
    terminal: &AcceptedVerificationJobOutcomeFact,
    evidence_created_at_millis: u64,
) -> Result<FencedExecutionIdentity, EvidenceResolutionError> {
    let worker_session_id = binding
        .worker_session_id
        .as_ref()
        .expect("complete binding");
    let codex_thread_id = binding.codex_thread_id.as_ref().expect("complete binding");
    if terminal.product_session_id() != &binding.product_session_id
        || terminal.execution_job_id() != &binding.execution_job_id
        || terminal.worker_session_id() != worker_session_id
        || terminal.codex_thread_id() != codex_thread_id
        || terminal.stage_run_id() != &stage_run.id
        || terminal.role_id() != stage_run.role
        || terminal.attempt() != stage_run.attempt
    {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SessionMismatch,
            "runtime terminal result belongs to another product, job, worker, thread, stage, or role",
        ));
    }
    if terminal.terminal_candidate_tree_id() != candidate.candidate_tree_id() {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::CandidateMismatch,
            "runtime terminal result belongs to another candidate tree",
        ));
    }
    let terminal_sequence = accepted_terminal_sequence(terminal);
    if terminal_sequence == 0
        || terminal_sequence > MAX_SAFE_INTEGER
        || terminal.finished_at_millis() > MAX_SAFE_INTEGER
        || stage_run.finished_at_millis != Some(terminal.finished_at_millis())
        || terminal.finished_at_millis() < stage_run.started_at_millis
        || terminal.finished_at_millis() < binding.bound_at_millis
        || evidence_created_at_millis < terminal.finished_at_millis()
    {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SourceTimeMismatch,
            "runtime terminal result must close this StageRun before Evidence is created",
        ));
    }
    Ok(FencedExecutionIdentity {
        product_session_id: terminal.product_session_id().clone(),
        execution_job_id: terminal.execution_job_id().clone(),
        stage_run_id: terminal.stage_run_id().clone(),
        role_id: terminal.role_id().into(),
        attempt: terminal.attempt(),
        lease_id: terminal.lease_id().clone(),
        fencing_token: terminal.fencing_token().clone(),
        worker_id: terminal.worker_id().clone(),
        worker_instance_id: terminal.worker_instance_id().clone(),
        worker_session_id: terminal.worker_session_id().clone(),
        codex_thread_id: terminal.codex_thread_id().clone(),
    })
}

fn validate_checkout_attestation(
    candidate: &FrozenDeliveryCandidate,
    checkout: &ValidatedCheckoutAttestationFact,
    expected_identity: &FencedExecutionIdentity,
) -> Result<(), EvidenceResolutionError> {
    if checkout_identity(checkout) != *expected_identity {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SessionMismatch,
            "checkout attestation belongs to another fenced execution",
        ));
    }
    if checkout.repository != *candidate.repository()
        || checkout.checkout_commit_id != candidate.candidate_commit_id()
        || checkout.checkout_tree_id != candidate.candidate_tree_id()
    {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::CandidateMismatch,
            "runtime source did not execute from the frozen candidate checkout",
        ));
    }
    Ok(())
}

fn runtime_source_identity(fact: &AcceptedRuntimeSourceFact) -> FencedExecutionIdentity {
    FencedExecutionIdentity {
        product_session_id: fact.product_session_id.clone(),
        execution_job_id: fact.execution_job_id.clone(),
        stage_run_id: fact.stage_run_id.clone(),
        role_id: fact.role_id.clone(),
        attempt: fact.attempt,
        lease_id: fact.lease_id.clone(),
        fencing_token: fact.fencing_token.clone(),
        worker_id: fact.worker_id.clone(),
        worker_instance_id: fact.worker_instance_id.clone(),
        worker_session_id: fact.worker_session_id.clone(),
        codex_thread_id: fact.codex_thread_id.clone(),
    }
}

fn terminal_identity(terminal: &AcceptedVerificationJobOutcomeFact) -> FencedExecutionIdentity {
    FencedExecutionIdentity {
        product_session_id: terminal.product_session_id().clone(),
        execution_job_id: terminal.execution_job_id().clone(),
        stage_run_id: terminal.stage_run_id().clone(),
        role_id: terminal.role_id().into(),
        attempt: terminal.attempt(),
        lease_id: terminal.lease_id().clone(),
        fencing_token: terminal.fencing_token().clone(),
        worker_id: terminal.worker_id().clone(),
        worker_instance_id: terminal.worker_instance_id().clone(),
        worker_session_id: terminal.worker_session_id().clone(),
        codex_thread_id: terminal.codex_thread_id().clone(),
    }
}

fn checkout_identity(fact: &ValidatedCheckoutAttestationFact) -> FencedExecutionIdentity {
    FencedExecutionIdentity {
        product_session_id: fact.product_session_id.clone(),
        execution_job_id: fact.execution_job_id.clone(),
        stage_run_id: fact.stage_run_id.clone(),
        role_id: fact.role_id.clone(),
        attempt: fact.attempt,
        lease_id: fact.lease_id.clone(),
        fencing_token: fact.fencing_token.clone(),
        worker_id: fact.worker_id.clone(),
        worker_instance_id: fact.worker_instance_id.clone(),
        worker_session_id: fact.worker_session_id.clone(),
        codex_thread_id: fact.codex_thread_id.clone(),
    }
}

fn validate_runtime_source_position(
    fact: &AcceptedRuntimeSourceFact,
    stage_run: &StageRun,
    binding: &SessionBinding,
    terminal: &AcceptedVerificationJobOutcomeFact,
    evidence_created_at_millis: u64,
) -> Result<(), EvidenceResolutionError> {
    if fact.source_sequence == 0
        || fact.source_sequence > MAX_SAFE_INTEGER
        || fact.occurred_at_millis > MAX_SAFE_INTEGER
        || fact.occurred_at_millis < stage_run.started_at_millis
        || fact.occurred_at_millis < binding.bound_at_millis
        || fact.source_sequence > accepted_terminal_sequence(terminal)
        || fact.occurred_at_millis > terminal.finished_at_millis()
        || fact.occurred_at_millis >= evidence_created_at_millis
    {
        return Err(resolution_error(
            EvidenceResolutionErrorCode::SourceTimeMismatch,
            "runtime source must follow its binding and strictly precede Evidence",
        ));
    }
    Ok(())
}

fn accepted_terminal_sequence(terminal: &AcceptedVerificationJobOutcomeFact) -> u64 {
    u64::try_from(terminal.last_event_sequence().0)
        .expect("accepted verification sequence is non-negative")
}

pub(crate) fn validate(evidence: &EvidenceRef, path: &str) -> Result<(), DeliveryValidationError> {
    schema_version(evidence.schema_version, &format!("{path}.schemaVersion"))?;
    portable_identifier(&evidence.id.0, &format!("{path}.id"))?;
    portable_identifier(&evidence.delivery_id.0, &format!("{path}.deliveryId"))?;
    portable_identifier(
        &evidence.delivery_spec_id.0,
        &format!("{path}.deliverySpecId"),
    )?;
    positive(
        evidence.delivery_spec_revision,
        &format!("{path}.deliverySpecRevision"),
    )?;
    portable_identifier(&evidence.stage_run_id.0, &format!("{path}.stageRunId"))?;
    portable_identifier(
        &evidence.session_binding_id.0,
        &format!("{path}.sessionBindingId"),
    )?;
    bounded_text(
        &evidence.candidate_ref,
        &format!("{path}.candidateRef"),
        MAX_REFERENCE_LENGTH,
    )?;
    bounded_text(
        &evidence.source_ref,
        &format!("{path}.sourceRef"),
        MAX_REFERENCE_LENGTH,
    )?;
    safe_non_negative(
        evidence.created_at_millis,
        &format!("{path}.createdAtMillis"),
    )
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::domain::verification::test_support::{
        VerificationFixtureState, independent_verification,
    };

    /// Resolves one current role-scoped runtime Evidence through the production
    /// resolver. Cross-module tests never construct [`ResolvedDeliveryEvidence`]
    /// or any accepted source fact directly.
    pub(crate) fn resolved_role_evidence(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
        role_id: &str,
        evidence_type: EvidenceRefType,
        outcome: VerifiedEvidenceOutcome,
        evidence_id: EvidenceId,
    ) -> ResolvedDeliveryEvidence {
        resolved_role_evidence_at_sequence(
            delivery,
            candidate,
            role_id,
            evidence_type,
            outcome,
            evidence_id,
            1,
        )
    }

    /// Builds a sealed runtime source at a selected accepted-ledger position.
    /// This narrow fixture lets verdict tests prove that a source reference
    /// alone cannot substitute for the finding's exact source position.
    pub(crate) fn resolved_role_evidence_at_sequence(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
        role_id: &str,
        evidence_type: EvidenceRefType,
        outcome: VerifiedEvidenceOutcome,
        evidence_id: EvidenceId,
        source_sequence: u64,
    ) -> ResolvedDeliveryEvidence {
        let stage_run = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| run.role == role_id && run.actor_type == StageRunActorType::Codex)
            .expect("fixture role StageRun");
        let binding = delivery
            .snapshot()
            .session_bindings
            .iter()
            .find(|binding| binding.stage_run_id == stage_run.id)
            .expect("fixture role SessionBinding");
        let (reviewer, verifier) = match role_id {
            "reviewer" => (
                VerificationFixtureState::SettledPass,
                VerificationFixtureState::Missing,
            ),
            "verifier" => (
                VerificationFixtureState::Missing,
                VerificationFixtureState::SettledPass,
            ),
            _ => panic!("Evidence fixture supports reviewer or verifier"),
        };
        let verification = independent_verification(delivery, candidate, reviewer, verifier);
        let terminal = verification
            .settlements()
            .iter()
            .find_map(|settlement| settlement.terminal_job_outcome())
            .filter(|terminal| terminal.role_id() == role_id)
            .expect("fixture accepted terminal outcome");
        let finished_at_millis = terminal.finished_at_millis();
        let source_event_id = ExecutionEventId(format!("event-evidence-{}", evidence_id.0));
        let source = AcceptedRuntimeSourceFact {
            source_event_id: source_event_id.clone(),
            evidence_type,
            product_session_id: terminal.product_session_id().clone(),
            execution_job_id: terminal.execution_job_id().clone(),
            worker_session_id: terminal.worker_session_id().clone(),
            codex_thread_id: terminal.codex_thread_id().clone(),
            stage_run_id: terminal.stage_run_id().clone(),
            role_id: terminal.role_id().into(),
            attempt: terminal.attempt(),
            lease_id: terminal.lease_id().clone(),
            fencing_token: terminal.fencing_token().clone(),
            worker_id: terminal.worker_id().clone(),
            worker_instance_id: terminal.worker_instance_id().clone(),
            source_sequence,
            candidate_ref: candidate.candidate_ref().into(),
            occurred_at_millis: finished_at_millis,
            outcome,
        };
        let checkout = ValidatedCheckoutAttestationFact {
            product_session_id: terminal.product_session_id().clone(),
            execution_job_id: terminal.execution_job_id().clone(),
            stage_run_id: terminal.stage_run_id().clone(),
            role_id: terminal.role_id().into(),
            attempt: terminal.attempt(),
            lease_id: terminal.lease_id().clone(),
            fencing_token: terminal.fencing_token().clone(),
            worker_id: terminal.worker_id().clone(),
            worker_instance_id: terminal.worker_instance_id().clone(),
            worker_session_id: terminal.worker_session_id().clone(),
            codex_thread_id: terminal.codex_thread_id().clone(),
            repository: candidate.repository().clone(),
            checkout_commit_id: candidate.candidate_commit_id().into(),
            checkout_tree_id: candidate.candidate_tree_id().into(),
        };
        let created_at_millis = finished_at_millis
            .checked_add(1)
            .expect("fixture Evidence timestamp");
        resolve_delivery_evidence(
            delivery,
            candidate,
            ResolveDeliveryEvidenceInput {
                evidence_id,
                stage_run_id: stage_run.id.clone(),
                session_binding_id: binding.id.clone(),
                source: EvidenceSource::Runtime {
                    evidence_type,
                    source_event_id,
                    accepted_sources: &[source],
                    terminal,
                    checkout: &checkout,
                },
                created_at_millis,
            },
        )
        .expect("fixture resolved current Evidence")
    }
}

#[cfg(test)]
mod tests {
    use winwincode_domain::{
        CodexThreadId, ExecutionEventId, ExecutionJobId, FencingToken, LeaseId, ProductSessionId,
        StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
    };

    use super::*;
    use crate::domain::{
        Delivery, DeliveryStage, DeliveryStatus, SessionBinding, SessionBindingId, StageRun,
        StageRunActorType, StageRunStatus,
        candidate::test_support::{freeze_facts, frozen_candidate, validated_git_snapshot},
        freeze_delivery_candidate, test_fixture,
        verification::test_support::{VerificationFixtureState, independent_verification},
    };

    fn evidence_delivery() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;

        let producer_task_id = {
            let producer = &mut snapshot.stage_runs[0];
            producer.id = StageRunId("stage-executor-1".into());
            producer.stage = DeliveryStage::Executing;
            producer.role = "executor".into();
            producer.status = StageRunStatus::Succeeded;
            producer.started_at_millis = 1_800_000_000_010;
            producer.finished_at_millis = Some(1_800_000_000_020);
            producer.delivery_task_id.clone()
        };

        let producer_binding = &mut snapshot.session_bindings[0];
        producer_binding.id = SessionBindingId("binding-executor-1".into());
        producer_binding.stage_run_id = StageRunId("stage-executor-1".into());
        producer_binding.product_session_id = ProductSessionId("product-executor".into());
        producer_binding.execution_job_id = ExecutionJobId("job-executor".into());
        producer_binding.worker_session_id = Some(WorkerSessionId("worker-executor".into()));
        producer_binding.codex_thread_id = Some(CodexThreadId("thread-executor".into()));
        producer_binding.bound_at_millis = 1_800_000_000_011;

        snapshot.stage_runs.push(StageRun {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: StageRunId("stage-verifier-1".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: producer_task_id.clone(),
            stage: DeliveryStage::Verifying,
            actor_type: StageRunActorType::Codex,
            role: "verifier".into(),
            status: StageRunStatus::Succeeded,
            attempt: 1,
            started_at_millis: 1_800_000_000_030,
            finished_at_millis: Some(1_800_000_000_050),
        });
        snapshot.stage_runs.push(StageRun {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: StageRunId("stage-reviewer-1".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: producer_task_id.clone(),
            stage: DeliveryStage::Verifying,
            actor_type: StageRunActorType::Codex,
            role: "reviewer".into(),
            status: StageRunStatus::Succeeded,
            attempt: 1,
            started_at_millis: 1_800_000_000_030,
            finished_at_millis: Some(1_800_000_000_050),
        });
        snapshot.session_bindings.push(SessionBinding {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId("binding-verifier-1".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: producer_task_id.clone(),
            stage_run_id: StageRunId("stage-verifier-1".into()),
            product_session_id: ProductSessionId("product-verifier".into()),
            execution_job_id: ExecutionJobId("job-verifier".into()),
            worker_session_id: Some(WorkerSessionId("worker-verifier".into())),
            codex_thread_id: Some(CodexThreadId("thread-verifier".into())),
            bound_at_millis: 1_800_000_000_031,
        });
        snapshot.session_bindings.push(SessionBinding {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId("binding-reviewer-1".into()),
            delivery_id: snapshot.id.clone(),
            delivery_task_id: producer_task_id,
            stage_run_id: StageRunId("stage-reviewer-1".into()),
            product_session_id: ProductSessionId("product-reviewer".into()),
            execution_job_id: ExecutionJobId("job-reviewer".into()),
            worker_session_id: Some(WorkerSessionId("worker-reviewer".into())),
            codex_thread_id: Some(CodexThreadId("thread-reviewer".into())),
            bound_at_millis: 1_800_000_000_031,
        });
        snapshot.updated_at_millis = 1_800_000_000_050;
        Delivery::try_from_snapshot(snapshot).expect("evidence Delivery")
    }

    fn candidate(delivery: &Delivery) -> FrozenDeliveryCandidate {
        frozen_candidate(
            delivery,
            &StageRunId("stage-executor-1".into()),
            &SessionBindingId("binding-executor-1".into()),
        )
    }

    fn candidate_with_tree(
        delivery: &Delivery,
        candidate_tree_id: &str,
    ) -> FrozenDeliveryCandidate {
        let current = candidate(delivery);
        let snapshot = validated_git_snapshot(
            delivery,
            current.producer_stage_run_id(),
            current.producer_session_binding_id(),
            current.candidate_commit_id(),
            candidate_tree_id,
            current.diff_sha256(),
            current.changed_paths().to_vec(),
        );
        freeze_delivery_candidate(delivery, &freeze_facts(delivery, snapshot))
            .expect("frozen candidate with alternate tree")
    }

    fn accepted_runtime_fact(
        candidate: &FrozenDeliveryCandidate,
        terminal: &AcceptedVerificationJobOutcomeFact,
    ) -> AcceptedRuntimeSourceFact {
        AcceptedRuntimeSourceFact {
            source_event_id: ExecutionEventId("event-verifier-test-7".into()),
            evidence_type: EvidenceRefType::Test,
            product_session_id: terminal.product_session_id().clone(),
            execution_job_id: terminal.execution_job_id().clone(),
            worker_session_id: terminal.worker_session_id().clone(),
            codex_thread_id: terminal.codex_thread_id().clone(),
            stage_run_id: terminal.stage_run_id().clone(),
            role_id: terminal.role_id().into(),
            attempt: terminal.attempt(),
            lease_id: terminal.lease_id().clone(),
            fencing_token: terminal.fencing_token().clone(),
            worker_id: terminal.worker_id().clone(),
            worker_instance_id: terminal.worker_instance_id().clone(),
            source_sequence: 1,
            candidate_ref: candidate.candidate_ref().into(),
            occurred_at_millis: 1_800_000_000_040,
            outcome: VerifiedEvidenceOutcome::Succeeded,
        }
    }

    fn checkout_attestation(
        candidate: &FrozenDeliveryCandidate,
        terminal: &AcceptedVerificationJobOutcomeFact,
    ) -> ValidatedCheckoutAttestationFact {
        ValidatedCheckoutAttestationFact {
            product_session_id: terminal.product_session_id().clone(),
            execution_job_id: terminal.execution_job_id().clone(),
            stage_run_id: terminal.stage_run_id().clone(),
            role_id: terminal.role_id().into(),
            attempt: terminal.attempt(),
            lease_id: terminal.lease_id().clone(),
            fencing_token: terminal.fencing_token().clone(),
            worker_id: terminal.worker_id().clone(),
            worker_instance_id: terminal.worker_instance_id().clone(),
            worker_session_id: terminal.worker_session_id().clone(),
            codex_thread_id: terminal.codex_thread_id().clone(),
            repository: candidate.repository().clone(),
            checkout_commit_id: candidate.candidate_commit_id().into(),
            checkout_tree_id: candidate.candidate_tree_id().into(),
        }
    }

    fn role_terminal(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
        role_id: &str,
    ) -> AcceptedVerificationJobOutcomeFact {
        let verification = independent_verification(
            delivery,
            candidate,
            VerificationFixtureState::SettledPass,
            VerificationFixtureState::SettledPass,
        );
        verification
            .settlements()
            .iter()
            .filter_map(|settlement| settlement.terminal_job_outcome())
            .find(|terminal| terminal.role_id() == role_id)
            .expect("accepted role terminal")
            .clone()
    }

    fn terminal(
        delivery: &Delivery,
        candidate: &FrozenDeliveryCandidate,
    ) -> AcceptedVerificationJobOutcomeFact {
        role_terminal(delivery, candidate, "verifier")
    }

    fn runtime_input<'facts>(
        accepted_sources: &'facts [AcceptedRuntimeSourceFact],
        terminal: &'facts AcceptedVerificationJobOutcomeFact,
        checkout: &'facts ValidatedCheckoutAttestationFact,
    ) -> ResolveDeliveryEvidenceInput<'facts> {
        ResolveDeliveryEvidenceInput {
            evidence_id: EvidenceId("evidence-runtime-1".into()),
            stage_run_id: StageRunId("stage-verifier-1".into()),
            session_binding_id: SessionBindingId("binding-verifier-1".into()),
            source: EvidenceSource::Runtime {
                evidence_type: EvidenceRefType::Test,
                source_event_id: ExecutionEventId("event-verifier-test-7".into()),
                accepted_sources,
                terminal,
                checkout,
            },
            created_at_millis: 1_800_000_000_060,
        }
    }

    fn direct_input(source: EvidenceSource<'static>) -> ResolveDeliveryEvidenceInput<'static> {
        ResolveDeliveryEvidenceInput {
            evidence_id: EvidenceId("evidence-direct-1".into()),
            stage_run_id: StageRunId("stage-executor-1".into()),
            session_binding_id: SessionBindingId("binding-executor-1".into()),
            source,
            created_at_millis: 1_800_000_000_025,
        }
    }

    #[test]
    fn evidence_matches_current_spec_revision() {
        let mut fixture = test_fixture();
        fixture.evidence[0].delivery_spec_revision += 1;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn evidence_matches_current_candidate() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let mut revised = delivery.snapshot().clone();
        revised.spec.revision += 1;
        revised.updated_at_millis += 1;
        let revised = Delivery::try_from_snapshot(revised).expect("revised Delivery");

        let error = resolve_delivery_evidence(
            &revised,
            &candidate,
            direct_input(EvidenceSource::CandidateCommit),
        )
        .expect_err("stale candidate");

        assert_eq!(error.code(), EvidenceResolutionErrorCode::CandidateStale);
    }

    #[test]
    fn evidence_matches_current_stage_run() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let accepted_sources = [accepted_runtime_fact(&candidate, &terminal)];
        let checkout = checkout_attestation(&candidate, &terminal);
        let mut input = runtime_input(&accepted_sources, &terminal, &checkout);
        input.stage_run_id = StageRunId("stage-foreign".into());

        let error =
            resolve_delivery_evidence(&delivery, &candidate, input).expect_err("foreign StageRun");

        assert_eq!(error.code(), EvidenceResolutionErrorCode::StageMismatch);
    }

    #[test]
    fn evidence_matches_current_session_binding() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let accepted_sources = [accepted_runtime_fact(&candidate, &terminal)];
        let checkout = checkout_attestation(&candidate, &terminal);
        let mut input = runtime_input(&accepted_sources, &terminal, &checkout);
        input.session_binding_id = SessionBindingId("binding-foreign".into());

        let error = resolve_delivery_evidence(&delivery, &candidate, input)
            .expect_err("foreign SessionBinding");

        assert_eq!(error.code(), EvidenceResolutionErrorCode::SessionMismatch);
    }

    #[test]
    fn evidence_matches_existing_stage_run() {
        let mut fixture = test_fixture();
        fixture.evidence[0].stage_run_id = StageRunId("foreign".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn evidence_matches_stage_run_session_binding() {
        let mut fixture = test_fixture();
        fixture.evidence[0].session_binding_id = SessionBindingId("foreign".into());
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn evidence_does_not_predate_run_or_binding() {
        let mut fixture = test_fixture();
        fixture.evidence[0].created_at_millis = fixture.session_bindings[0].bound_at_millis - 1;
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn criterion_evidence_matches_current_candidate() {
        let mut fixture = test_fixture();
        fixture.evidence[0].candidate_ref =
            "git-tree:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        assert!(Delivery::try_from_snapshot(fixture).is_err());
    }

    #[test]
    fn evidence_resolves_one_exact_runtime_source_identity() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let accepted_sources = [accepted_runtime_fact(&candidate, &terminal)];
        let checkout = checkout_attestation(&candidate, &terminal);

        let resolved = resolve_delivery_evidence(
            &delivery,
            &candidate,
            runtime_input(&accepted_sources, &terminal, &checkout),
        )
        .expect("exact runtime source");
        let evidence = resolved.evidence();

        assert_eq!(evidence.delivery_id, *delivery.id());
        assert_eq!(evidence.delivery_spec_id, delivery.snapshot().spec.id);
        assert_eq!(
            evidence.delivery_spec_revision,
            delivery.snapshot().spec.revision
        );
        assert_eq!(evidence.stage_run_id.0, "stage-verifier-1");
        assert_eq!(evidence.session_binding_id.0, "binding-verifier-1");
        assert_eq!(evidence.candidate_ref, candidate.candidate_ref());
        assert_eq!(evidence.evidence_type, EvidenceRefType::Test);
        assert_eq!(evidence.source_ref, "runtime_event:event-verifier-test-7");
        assert_eq!(resolved.outcome(), VerifiedEvidenceOutcome::Succeeded);
    }

    #[test]
    fn evidence_requires_sealed_accepted_runtime_source_fact() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let checkout = checkout_attestation(&candidate, &terminal);

        let error = resolve_delivery_evidence(
            &delivery,
            &candidate,
            runtime_input(&[], &terminal, &checkout),
        )
        .expect_err("unpersisted source position");

        assert_eq!(error.code(), EvidenceResolutionErrorCode::SourceMissing);
    }

    #[test]
    fn runtime_evidence_requires_sealed_exact_candidate_checkout_attestation() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let sources = [accepted_runtime_fact(&candidate, &terminal)];
        let valid = checkout_attestation(&candidate, &terminal);
        let mut foreign_attestations = Vec::new();

        let mut commit = valid.clone();
        commit.checkout_commit_id = "5555555555555555555555555555555555555555".into();
        foreign_attestations.push(commit);
        let mut tree = valid.clone();
        tree.checkout_tree_id = "5555555555555555555555555555555555555555".into();
        foreign_attestations.push(tree);
        for attestation in &foreign_attestations {
            let error = resolve_delivery_evidence(
                &delivery,
                &candidate,
                runtime_input(&sources, &terminal, attestation),
            )
            .expect_err("foreign checkout attestation");
            assert_eq!(error.code(), EvidenceResolutionErrorCode::CandidateMismatch);
        }

        let mut foreign_job = valid;
        foreign_job.execution_job_id = ExecutionJobId("job-foreign".into());
        let error = resolve_delivery_evidence(
            &delivery,
            &candidate,
            runtime_input(&sources, &terminal, &foreign_job),
        )
        .expect_err("foreign checkout job attestation");
        assert_eq!(error.code(), EvidenceResolutionErrorCode::SessionMismatch);
    }

    #[test]
    fn rejects_source_before_stage_or_binding_and_after_terminal_sequence() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let checkout = checkout_attestation(&candidate, &terminal);
        let valid = accepted_runtime_fact(&candidate, &terminal);

        let mut before_stage = valid.clone();
        before_stage.occurred_at_millis = 1_800_000_000_029;
        let mut before_binding = valid.clone();
        before_binding.occurred_at_millis = 1_800_000_000_030;
        let mut after_terminal_sequence = valid.clone();
        after_terminal_sequence.source_sequence = accepted_terminal_sequence(&terminal) + 1;
        let mut after_terminal_time = valid;
        after_terminal_time.occurred_at_millis = terminal.finished_at_millis() + 1;

        for source in [
            before_stage,
            before_binding,
            after_terminal_sequence,
            after_terminal_time,
        ] {
            let error = resolve_delivery_evidence(
                &delivery,
                &candidate,
                runtime_input(&[source], &terminal, &checkout),
            )
            .expect_err("source outside accepted time or sequence range");
            assert_eq!(
                error.code(),
                EvidenceResolutionErrorCode::SourceTimeMismatch
            );
        }
    }

    #[test]
    fn runtime_source_identity_rejects_every_foreign_identity_component() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let checkout = checkout_attestation(&candidate, &terminal);
        let valid = accepted_runtime_fact(&candidate, &terminal);
        let mut cases = Vec::new();

        let mut product = valid.clone();
        product.product_session_id = ProductSessionId("product-foreign".into());
        cases.push((product, EvidenceResolutionErrorCode::SessionMismatch));
        let mut job = valid.clone();
        job.execution_job_id = ExecutionJobId("job-foreign".into());
        cases.push((job, EvidenceResolutionErrorCode::SessionMismatch));
        let mut worker_session = valid.clone();
        worker_session.worker_session_id = WorkerSessionId("worker-foreign".into());
        cases.push((worker_session, EvidenceResolutionErrorCode::SessionMismatch));
        let mut thread = valid.clone();
        thread.codex_thread_id = CodexThreadId("thread-foreign".into());
        cases.push((thread, EvidenceResolutionErrorCode::SessionMismatch));
        let mut stage = valid.clone();
        stage.stage_run_id = StageRunId("stage-foreign".into());
        cases.push((stage, EvidenceResolutionErrorCode::SessionMismatch));
        let mut role = valid.clone();
        role.role_id = "reviewer".into();
        cases.push((role, EvidenceResolutionErrorCode::SessionMismatch));
        let mut attempt = valid.clone();
        attempt.attempt += 1;
        cases.push((attempt, EvidenceResolutionErrorCode::SessionMismatch));
        let mut lease = valid.clone();
        lease.lease_id = LeaseId("lease-foreign".into());
        cases.push((lease, EvidenceResolutionErrorCode::SessionMismatch));
        let mut fence = valid.clone();
        fence.fencing_token = FencingToken("8".into());
        cases.push((fence, EvidenceResolutionErrorCode::SessionMismatch));
        let mut worker = valid.clone();
        worker.worker_id = WorkerId("worker-node-foreign".into());
        cases.push((worker, EvidenceResolutionErrorCode::SessionMismatch));
        let mut instance = valid.clone();
        instance.worker_instance_id = WorkerInstanceId("worker-instance-foreign".into());
        cases.push((instance, EvidenceResolutionErrorCode::SessionMismatch));
        let mut candidate_ref = valid;
        candidate_ref.candidate_ref = "git-candidate:sha256:deadbeef".into();
        cases.push((
            candidate_ref,
            EvidenceResolutionErrorCode::CandidateMismatch,
        ));

        for (source, expected) in cases {
            let error = resolve_delivery_evidence(
                &delivery,
                &candidate,
                runtime_input(&[source], &terminal, &checkout),
            )
            .expect_err("foreign accepted source identity");
            assert_eq!(error.code(), expected, "{}", error.message());
        }
    }

    #[test]
    fn runtime_source_rejects_duplicate_missing_wrong_type_and_wrong_position() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let checkout = checkout_attestation(&candidate, &terminal);
        let valid = accepted_runtime_fact(&candidate, &terminal);

        let duplicate = [valid.clone(), valid.clone()];
        let duplicate_error = resolve_delivery_evidence(
            &delivery,
            &candidate,
            runtime_input(&duplicate, &terminal, &checkout),
        )
        .expect_err("duplicate accepted source");
        assert_eq!(
            duplicate_error.code(),
            EvidenceResolutionErrorCode::SourceAmbiguous
        );

        let mut wrong_type = valid.clone();
        wrong_type.evidence_type = EvidenceRefType::Command;
        let type_error = resolve_delivery_evidence(
            &delivery,
            &candidate,
            runtime_input(&[wrong_type], &terminal, &checkout),
        )
        .expect_err("wrong source type");
        assert_eq!(type_error.code(), EvidenceResolutionErrorCode::TypeMismatch);

        let wrong_position_sources = [valid];
        let mut wrong_position_input = runtime_input(&wrong_position_sources, &terminal, &checkout);
        if let EvidenceSource::Runtime {
            source_event_id, ..
        } = &mut wrong_position_input.source
        {
            *source_event_id = ExecutionEventId("event-missing".into());
        }
        let missing_error = resolve_delivery_evidence(&delivery, &candidate, wrong_position_input)
            .expect_err("missing accepted source position");
        assert_eq!(
            missing_error.code(),
            EvidenceResolutionErrorCode::SourceMissing
        );
    }

    #[test]
    fn checkout_attestation_rejects_foreign_fenced_execution_identity() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let sources = [accepted_runtime_fact(&candidate, &terminal)];
        let valid = checkout_attestation(&candidate, &terminal);
        let mut cases = Vec::new();

        let mut product = valid.clone();
        product.product_session_id = ProductSessionId("product-foreign".into());
        cases.push(product);
        let mut stage = valid.clone();
        stage.stage_run_id = StageRunId("stage-foreign".into());
        cases.push(stage);
        let mut role = valid.clone();
        role.role_id = "reviewer".into();
        cases.push(role);
        let mut attempt = valid.clone();
        attempt.attempt += 1;
        cases.push(attempt);
        let mut lease = valid.clone();
        lease.lease_id = LeaseId("lease-foreign".into());
        cases.push(lease);
        let mut fence = valid.clone();
        fence.fencing_token = FencingToken("8".into());
        cases.push(fence);
        let mut worker = valid.clone();
        worker.worker_id = WorkerId("worker-node-foreign".into());
        cases.push(worker);
        let mut instance = valid.clone();
        instance.worker_instance_id = WorkerInstanceId("worker-instance-foreign".into());
        cases.push(instance);
        let mut worker_session = valid.clone();
        worker_session.worker_session_id = WorkerSessionId("worker-session-foreign".into());
        cases.push(worker_session);
        let mut thread = valid;
        thread.codex_thread_id = CodexThreadId("thread-foreign".into());
        cases.push(thread);

        for checkout in &cases {
            let error = resolve_delivery_evidence(
                &delivery,
                &candidate,
                runtime_input(&sources, &terminal, checkout),
            )
            .expect_err("foreign checkout execution identity");
            assert_eq!(error.code(), EvidenceResolutionErrorCode::SessionMismatch);
        }
    }

    #[test]
    fn accepted_terminal_identity_and_candidate_tree_are_exact() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let accepted_terminal = terminal(&delivery, &candidate);
        let sources = [accepted_runtime_fact(&candidate, &accepted_terminal)];
        let checkout = checkout_attestation(&candidate, &accepted_terminal);

        let foreign_identity = role_terminal(&delivery, &candidate, "reviewer");
        let error = resolve_delivery_evidence(
            &delivery,
            &candidate,
            runtime_input(&sources, &foreign_identity, &checkout),
        )
        .expect_err("foreign accepted terminal identity");
        assert_eq!(error.code(), EvidenceResolutionErrorCode::SessionMismatch);

        let foreign_candidate =
            candidate_with_tree(&delivery, "5555555555555555555555555555555555555555");
        let foreign_tree = terminal(&delivery, &foreign_candidate);
        let error = resolve_delivery_evidence(
            &delivery,
            &candidate,
            runtime_input(&sources, &foreign_tree, &checkout),
        )
        .expect_err("foreign accepted terminal candidate tree");
        assert_eq!(error.code(), EvidenceResolutionErrorCode::CandidateMismatch);
    }

    #[test]
    fn sealed_runtime_source_outcome_is_preserved_without_event_payload() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);
        let terminal = terminal(&delivery, &candidate);
        let checkout = checkout_attestation(&candidate, &terminal);
        for outcome in [
            VerifiedEvidenceOutcome::Observed,
            VerifiedEvidenceOutcome::Succeeded,
            VerifiedEvidenceOutcome::Failed,
            VerifiedEvidenceOutcome::TimedOut,
            VerifiedEvidenceOutcome::PolicyDenied,
            VerifiedEvidenceOutcome::InfrastructureFailed,
            VerifiedEvidenceOutcome::Cancelled,
        ] {
            let mut source = accepted_runtime_fact(&candidate, &terminal);
            source.outcome = outcome;
            let resolved = resolve_delivery_evidence(
                &delivery,
                &candidate,
                runtime_input(&[source], &terminal, &checkout),
            )
            .expect("sealed source outcome");
            assert_eq!(resolved.outcome(), outcome);
            let evidence = serde_json::to_value(resolved.evidence()).expect("Evidence JSON");
            let object = evidence.as_object().expect("Evidence object");
            assert_eq!(object.len(), 11);
            assert!(!object.contains_key("log"));
            assert!(!object.contains_key("output"));
            assert!(!object.contains_key("runtimeEvent"));
        }
    }

    #[test]
    fn commit_evidence_is_rebuilt_from_the_frozen_candidate() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);

        let resolved = resolve_delivery_evidence(
            &delivery,
            &candidate,
            direct_input(EvidenceSource::CandidateCommit),
        )
        .expect("candidate commit evidence");

        assert_eq!(resolved.evidence().evidence_type, EvidenceRefType::Commit);
        assert_eq!(
            resolved.evidence().source_ref,
            "git_commit:2222222222222222222222222222222222222222"
        );
        assert_eq!(resolved.outcome(), VerifiedEvidenceOutcome::Observed);
        assert_eq!(
            resolved.source_identity(),
            &VerifiedEvidenceSourceIdentity::CandidateCommit {
                candidate_ref: candidate.candidate_ref().into(),
                candidate_commit_id: candidate.candidate_commit_id().into(),
            }
        );
    }

    #[test]
    fn diff_evidence_is_rebuilt_from_the_frozen_candidate() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);

        let resolved = resolve_delivery_evidence(
            &delivery,
            &candidate,
            direct_input(EvidenceSource::CandidateDiff),
        )
        .expect("candidate diff evidence");

        assert_eq!(resolved.evidence().evidence_type, EvidenceRefType::Diff);
        assert_eq!(
            resolved.evidence().source_ref,
            format!("git_diff:sha256:{}", "a".repeat(64))
        );
        assert_eq!(
            resolved.source_identity(),
            &VerifiedEvidenceSourceIdentity::CandidateDiff {
                candidate_ref: candidate.candidate_ref().into(),
                diff_sha256: candidate.diff_sha256().into(),
            }
        );
    }

    #[test]
    fn file_evidence_is_rebuilt_from_one_frozen_candidate_path() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);

        let resolved = resolve_delivery_evidence(
            &delivery,
            &candidate,
            direct_input(EvidenceSource::CandidateFile {
                path: "src/invitation.rs".into(),
            }),
        )
        .expect("candidate file evidence");

        assert_eq!(resolved.evidence().evidence_type, EvidenceRefType::File);
        assert_eq!(
            resolved.evidence().source_ref,
            concat!(
                "git_file:3333333333333333333333333333333333333333:",
                "src%2Finvitation.rs@4444444444444444444444444444444444444444"
            )
        );
        assert_eq!(
            resolved.source_identity(),
            &VerifiedEvidenceSourceIdentity::CandidateFile {
                candidate_ref: candidate.candidate_ref().into(),
                candidate_tree_id: candidate.candidate_tree_id().into(),
                path: "src/invitation.rs".into(),
                object_id: "4444444444444444444444444444444444444444".into(),
            }
        );
    }

    #[test]
    fn direct_git_evidence_rejects_missing_path_or_foreign_producer_identity() {
        let delivery = evidence_delivery();
        let candidate = candidate(&delivery);

        let missing = resolve_delivery_evidence(
            &delivery,
            &candidate,
            direct_input(EvidenceSource::CandidateFile {
                path: "src/missing.rs".into(),
            }),
        )
        .expect_err("missing candidate path");
        assert_eq!(missing.code(), EvidenceResolutionErrorCode::SourceMissing);

        let mut foreign_stage = direct_input(EvidenceSource::CandidateCommit);
        foreign_stage.stage_run_id = StageRunId("stage-verifier-1".into());
        foreign_stage.session_binding_id = SessionBindingId("binding-verifier-1".into());
        foreign_stage.created_at_millis = 1_800_000_000_060;
        let foreign = resolve_delivery_evidence(&delivery, &candidate, foreign_stage)
            .expect_err("foreign direct Git producer");
        assert_eq!(foreign.code(), EvidenceResolutionErrorCode::SessionMismatch);

        let mut early = direct_input(EvidenceSource::CandidateDiff);
        early.created_at_millis = 1_800_000_000_019;
        let early = resolve_delivery_evidence(&delivery, &candidate, early)
            .expect_err("early direct Git evidence");
        assert_eq!(
            early.code(),
            EvidenceResolutionErrorCode::SourceTimeMismatch
        );
    }
}
