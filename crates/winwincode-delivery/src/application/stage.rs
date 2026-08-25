// SPDX-License-Identifier: Apache-2.0

//! Delivery stage coordination application service.
//!
//! Raw Scheduler and Worker facts are deliberately not an external production
//! API while their authoritative Phase 4 adapters do not exist.
//!
//! ```compile_fail
//! use winwincode_delivery::application::stage::ActiveLeaseIdentity;
//!
//! let _caller_built_lease = ActiveLeaseIdentity {
//!     execution_job_id: todo!(),
//!     attempt: 1,
//!     lease_id: todo!(),
//!     fencing_token: todo!(),
//!     worker_id: todo!(),
//!     worker_instance_id: todo!(),
//!     worker_session_id: todo!(),
//! };
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::stage::SessionBindingAuthority;
//!
//! let _caller_built_authority = SessionBindingAuthority {
//!     active_lease: todo!(),
//!     issued_at: todo!(),
//!     expires_at: todo!(),
//! };
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::stage::SessionBindingAuthority;
//!
//! let _deserialized: SessionBindingAuthority = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::stage::TerminalWorkerOutcome;
//!
//! let _caller_built_outcome = TerminalWorkerOutcome {
//!     stage_run_id: todo!(),
//!     execution_job_id: todo!(),
//!     attempt: 1,
//!     lease_id: todo!(),
//!     fencing_token: todo!(),
//!     worker_id: todo!(),
//!     worker_instance_id: todo!(),
//!     worker_session_id: todo!(),
//!     status: todo!(),
//!     metadata: todo!(),
//! };
//! ```
//!
//! ```compile_fail
//! let _caller_callable_resolver =
//!     winwincode_delivery::application::stage::verify_terminal_outcome;
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts;
//!
//! let _caller_built_facts = DeliveryTerminalOutcomeFacts {
//!     authority: todo!(),
//!     outcome: todo!(),
//! };
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::stage::DeliveryTerminalOutcomeFacts;
//!
//! let _deserialized: DeliveryTerminalOutcomeFacts = serde_json::from_str("{}").unwrap();
//! ```

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, AttentionItemId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionAckSequence,
    ExecutionJobId, FencingToken, Instant, LeaseId, ProductSessionId, Sha256Digest, StageRunId,
    WorkerId, WorkerInstanceId, WorkerSessionId,
};

use crate::domain::{
    AttentionItem, AttentionItemStatus, AttentionItemType, DELIVERY_SCHEMA_VERSION, Delivery,
    DeliverySnapshot, DeliveryStage, DeliveryStatus, DeliveryTaskStatus, SessionBinding,
    SessionBindingId, StageRun, StageRunActorType, StageRunStatus,
    rework::{ReworkAuthorization, ReworkClarificationReason, ReworkDecision},
};
use crate::domain::{MAX_COLLECTION_LENGTH, MAX_SAFE_INTEGER};

use super::task::{TaskFact, runnable_task, transition_task_status};
use super::{CoordinationError, CoordinationErrorCode, require_mutation_time};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewStageIdentities {
    pub stage_run_id: StageRunId,
    pub execution_job_id: ExecutionJobId,
    pub session_binding_id: SessionBindingId,
    pub attention_item_id: AttentionItemId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewAttentionSeed {
    pub title: String,
    pub context: String,
    pub assigned_to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvanceStageInput {
    pub expected_revision: u64,
    pub product_session_id: ProductSessionId,
    pub identities: NewStageIdentities,
    pub review: Option<ReviewAttentionSeed>,
    pub previous_outcome: Option<VerifiedTerminalOutcome>,
    pub current_lease: Option<ActiveLeaseIdentity>,
    /// Exact current-candidate remediation authority. Required only for a
    /// `Reworking` stage and consumed into the immutable dispatch intent.
    pub rework_authorization: Option<Box<ReworkAuthorization>>,
    pub now_millis: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalOutcomeStatus {
    Succeeded,
    Failed,
    InfrastructureError,
    Cancelled,
}

/// Scheduler-owned lease identity loaded from durable Control Plane state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveLeaseIdentity {
    execution_job_id: ExecutionJobId,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
}

impl ActiveLeaseIdentity {
    pub fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    pub const fn attempt(&self) -> u64 {
        self.attempt
    }

    pub fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }

    pub fn fencing_token(&self) -> &FencingToken {
        &self.fencing_token
    }

    pub fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    pub fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.worker_instance_id
    }

    pub fn worker_session_id(&self) -> &WorkerSessionId {
        &self.worker_session_id
    }
}

/// Scheduler-owned authority for accepting one Worker `session.binding`.
///
/// The active lease identity alone does not contain its issued/expiry window.
/// This sealed fact binds that exact window to the scheduler-owned lease so a
/// Worker message cannot extend or replace its own authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingAuthority {
    active_lease: ActiveLeaseIdentity,
    issued_at: Instant,
    expires_at: Instant,
}

/// Scheduler- and Worker-adapter facts for one terminal `job.outcome`.
///
/// The raw lease and outcome fields stay private. A production adapter must
/// obtain this value from its trusted scheduler/Worker boundary; ordinary
/// callers cannot construct or deserialize terminal authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryTerminalOutcomeFacts {
    authority: SessionBindingAuthority,
    outcome: TerminalWorkerOutcome,
}

impl DeliveryTerminalOutcomeFacts {
    pub const fn authority(&self) -> &SessionBindingAuthority {
        &self.authority
    }

    pub const fn stage_run_id(&self) -> &StageRunId {
        &self.outcome.stage_run_id
    }

    pub const fn status(&self) -> TerminalOutcomeStatus {
        self.outcome.status
    }

    pub const fn metadata(&self) -> &TerminalOutcomeMetadata {
        &self.outcome.metadata
    }

    pub(crate) fn verify(
        &self,
        delivery: &Delivery,
    ) -> Result<VerifiedTerminalOutcome, CoordinationError> {
        verify_terminal_outcome(
            delivery,
            self.authority.active_lease(),
            self.outcome.clone(),
        )
    }
}

impl SessionBindingAuthority {
    pub const fn active_lease(&self) -> &ActiveLeaseIdentity {
        &self.active_lease
    }

    pub const fn issued_at(&self) -> &Instant {
        &self.issued_at
    }

    pub const fn expires_at(&self) -> &Instant {
        &self.expires_at
    }
}

/// Terminal fact reported by Worker through `ExecutionPort`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalWorkerOutcome {
    stage_run_id: StageRunId,
    execution_job_id: ExecutionJobId,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
    status: TerminalOutcomeStatus,
    metadata: TerminalOutcomeMetadata,
}

/// Bounded facts carried by the accepted Worker `job.outcome`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutcomeMetadata {
    codex_thread_id: Option<CodexThreadId>,
    finished_at_millis: u64,
    last_event_sequence: ExecutionAckSequence,
    artifacts: Vec<TerminalArtifactReference>,
}

impl TerminalOutcomeMetadata {
    pub fn codex_thread_id(&self) -> Option<&CodexThreadId> {
        self.codex_thread_id.as_ref()
    }

    pub const fn finished_at_millis(&self) -> u64 {
        self.finished_at_millis
    }

    pub fn last_event_sequence(&self) -> &ExecutionAckSequence {
        &self.last_event_sequence
    }

    pub fn artifacts(&self) -> &[TerminalArtifactReference] {
        &self.artifacts
    }
}

/// One immutable Artifact identity named by the accepted Worker outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalArtifactReference {
    pub artifact_id: ArtifactId,
    pub digest: Sha256Digest,
}

/// A terminal fact that matched the current `StageRun`, `SessionBinding`, and
/// scheduler lease/fencing identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTerminalOutcome {
    stage_run_id: StageRunId,
    lease_identity: ActiveLeaseIdentity,
    status: TerminalOutcomeStatus,
    metadata: TerminalOutcomeMetadata,
}

impl VerifiedTerminalOutcome {
    pub fn stage_run_id(&self) -> &StageRunId {
        &self.stage_run_id
    }

    pub fn execution_job_id(&self) -> &ExecutionJobId {
        &self.lease_identity.execution_job_id
    }

    pub fn worker_session_id(&self) -> &WorkerSessionId {
        &self.lease_identity.worker_session_id
    }

    pub fn lease_id(&self) -> &LeaseId {
        &self.lease_identity.lease_id
    }

    pub fn fencing_token(&self) -> &FencingToken {
        &self.lease_identity.fencing_token
    }

    pub fn worker_id(&self) -> &WorkerId {
        &self.lease_identity.worker_id
    }

    pub fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.lease_identity.worker_instance_id
    }

    pub const fn attempt(&self) -> u64 {
        self.lease_identity.attempt
    }

    pub const fn status(&self) -> TerminalOutcomeStatus {
        self.status
    }

    pub fn codex_thread_id(&self) -> Option<&CodexThreadId> {
        self.metadata.codex_thread_id.as_ref()
    }

    pub const fn finished_at_millis(&self) -> u64 {
        self.metadata.finished_at_millis
    }

    pub fn last_event_sequence(&self) -> &ExecutionAckSequence {
        &self.metadata.last_event_sequence
    }

    pub fn artifacts(&self) -> &[TerminalArtifactReference] {
        &self.metadata.artifacts
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn fixture_verified_terminal_outcome(
    stage_run_id: StageRunId,
    lease_identity: ActiveLeaseIdentity,
    status: TerminalOutcomeStatus,
    metadata: TerminalOutcomeMetadata,
) -> VerifiedTerminalOutcome {
    VerifiedTerminalOutcome {
        stage_run_id,
        lease_identity,
        status,
        metadata,
    }
}

/// Verifies a Worker terminal outcome against both Delivery and scheduler facts.
///
/// # Errors
///
/// Fails closed when any Delivery, job, attempt, Worker, lease, instance, or
/// fencing identity differs.
pub(crate) fn verify_terminal_outcome(
    delivery: &Delivery,
    lease: &ActiveLeaseIdentity,
    outcome: TerminalWorkerOutcome,
) -> Result<VerifiedTerminalOutcome, CoordinationError> {
    let mut active = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| is_active(run));
    let run = active.next().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "terminal outcome has no active StageRun",
        )
    })?;
    if active.next().is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "terminal outcome cannot choose among multiple active StageRuns",
        ));
    }
    let binding = exact_binding(delivery, run, true)?;
    validate_terminal_metadata(run, binding, &outcome.metadata)?;
    let exact = outcome.stage_run_id == run.id
        && outcome.execution_job_id == binding.execution_job_id
        && outcome.execution_job_id == lease.execution_job_id
        && outcome.attempt == run.attempt
        && outcome.attempt == lease.attempt
        && binding.worker_session_id.as_ref() == Some(&outcome.worker_session_id)
        && outcome.worker_session_id == lease.worker_session_id
        && outcome.lease_id == lease.lease_id
        && outcome.fencing_token == lease.fencing_token
        && outcome.worker_id == lease.worker_id
        && outcome.worker_instance_id == lease.worker_instance_id;
    if !exact {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "terminal Worker outcome does not match the active StageRun lease and SessionBinding",
        ));
    }
    Ok(VerifiedTerminalOutcome {
        stage_run_id: outcome.stage_run_id,
        lease_identity: lease.clone(),
        status: outcome.status,
        metadata: outcome.metadata,
    })
}

/// Construction helpers used only by Rust integration tests. Production
/// Control Plane builds do not enable this feature.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    use super::{
        ActiveLeaseIdentity, CodexThreadId, CoordinationError, Delivery,
        DeliveryTerminalOutcomeFacts, ExecutionAckSequence, ExecutionJobId, FencingToken, Instant,
        LeaseId, SessionBindingAuthority, Sha256Digest, StageRunId, TerminalArtifactReference,
        TerminalOutcomeMetadata, TerminalOutcomeStatus, TerminalWorkerOutcome,
        VerifiedTerminalOutcome, WorkerId, WorkerInstanceId, WorkerSessionId,
    };

    #[allow(clippy::too_many_arguments)]
    pub fn active_lease_identity(
        execution_job_id: ExecutionJobId,
        attempt: u64,
        lease_id: LeaseId,
        fencing_token: FencingToken,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        worker_session_id: WorkerSessionId,
    ) -> ActiveLeaseIdentity {
        ActiveLeaseIdentity {
            execution_job_id,
            attempt,
            lease_id,
            fencing_token,
            worker_id,
            worker_instance_id,
            worker_session_id,
        }
    }

    /// Seals one exact active-lease window for a `SessionBinding` integration
    /// fixture. Production schedulers construct the equivalent fact inside
    /// their trusted adapter; raw fields remain unavailable to callers.
    pub fn session_binding_authority(
        active_lease: ActiveLeaseIdentity,
        issued_at: Instant,
        expires_at: Instant,
    ) -> SessionBindingAuthority {
        SessionBindingAuthority {
            active_lease,
            issued_at,
            expires_at,
        }
    }

    /// Seals one scheduler lease and raw Worker outcome for Control Plane
    /// transaction tests. Production builds expose no equivalent constructor.
    pub fn delivery_terminal_outcome_facts(
        authority: SessionBindingAuthority,
        outcome: TerminalWorkerOutcome,
    ) -> DeliveryTerminalOutcomeFacts {
        DeliveryTerminalOutcomeFacts { authority, outcome }
    }

    pub fn terminal_outcome_metadata(
        codex_thread_id: Option<CodexThreadId>,
        finished_at_millis: u64,
        last_event_sequence: ExecutionAckSequence,
        artifacts: Vec<TerminalArtifactReference>,
    ) -> TerminalOutcomeMetadata {
        TerminalOutcomeMetadata {
            codex_thread_id,
            finished_at_millis,
            last_event_sequence,
            artifacts,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn terminal_worker_outcome(
        stage_run_id: StageRunId,
        execution_job_id: ExecutionJobId,
        attempt: u64,
        lease_id: LeaseId,
        fencing_token: FencingToken,
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        worker_session_id: WorkerSessionId,
        status: TerminalOutcomeStatus,
        metadata: TerminalOutcomeMetadata,
    ) -> TerminalWorkerOutcome {
        TerminalWorkerOutcome {
            stage_run_id,
            execution_job_id,
            attempt,
            lease_id,
            fencing_token,
            worker_id,
            worker_instance_id,
            worker_session_id,
            status,
            metadata,
        }
    }

    pub fn verify_terminal_outcome(
        delivery: &Delivery,
        lease: &ActiveLeaseIdentity,
        outcome: TerminalWorkerOutcome,
    ) -> Result<VerifiedTerminalOutcome, CoordinationError> {
        super::verify_terminal_outcome(delivery, lease, outcome)
    }

    pub fn set_terminal_codex_thread_id(
        outcome: &mut TerminalWorkerOutcome,
        codex_thread_id: Option<CodexThreadId>,
    ) {
        outcome.metadata.codex_thread_id = codex_thread_id;
    }

    pub fn set_terminal_last_event_sequence(
        outcome: &mut TerminalWorkerOutcome,
        sequence: ExecutionAckSequence,
    ) {
        outcome.metadata.last_event_sequence = sequence;
    }

    pub fn set_first_terminal_artifact_digest(
        outcome: &mut TerminalWorkerOutcome,
        digest: Sha256Digest,
    ) {
        outcome
            .metadata
            .artifacts
            .first_mut()
            .expect("terminal test fixture requires an Artifact")
            .digest = digest;
    }

    pub fn duplicate_first_terminal_artifact(outcome: &mut TerminalWorkerOutcome) {
        let duplicate = outcome
            .metadata
            .artifacts
            .first()
            .expect("terminal test fixture requires an Artifact")
            .clone();
        outcome.metadata.artifacts.push(duplicate);
    }

    pub fn terminal_metadata(outcome: &TerminalWorkerOutcome) -> &TerminalOutcomeMetadata {
        &outcome.metadata
    }
}

fn validate_terminal_metadata(
    run: &StageRun,
    binding: &SessionBinding,
    metadata: &TerminalOutcomeMetadata,
) -> Result<(), CoordinationError> {
    let max_sequence = i64::try_from(MAX_SAFE_INTEGER).unwrap_or(i64::MAX);
    if metadata.finished_at_millis > MAX_SAFE_INTEGER
        || metadata.finished_at_millis < run.started_at_millis
        || metadata.finished_at_millis < binding.bound_at_millis
        || !(0..=max_sequence).contains(&metadata.last_event_sequence.0)
        || metadata.codex_thread_id != binding.codex_thread_id
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "terminal Worker metadata does not match the StageRun time, CodexThread, or event sequence",
        ));
    }
    if metadata.artifacts.len() > MAX_COLLECTION_LENGTH {
        return Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "terminal Worker artifacts exceed the supported limit",
        ));
    }
    let mut artifact_ids = HashSet::with_capacity(metadata.artifacts.len());
    for artifact in &metadata.artifacts {
        let valid_digest = artifact
            .digest
            .0
            .strip_prefix("sha256:")
            .is_some_and(lowercase_sha256);
        if !portable_execution_identifier(&artifact.artifact_id.0)
            || !artifact_ids.insert(artifact.artifact_id.0.as_str())
            || !valid_digest
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::InvalidRequest,
                "terminal Worker artifacts must have unique identities and lowercase SHA-256 digests",
            ));
        }
    }
    Ok(())
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn portable_execution_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 200
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes.all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionIntent {
    pub execution_job_id: ExecutionJobId,
    pub product_session_id: ProductSessionId,
    pub delivery_id: DeliveryId,
    pub delivery_task_id: Option<DeliveryTaskId>,
    pub stage_run_id: StageRunId,
    pub stage: DeliveryStage,
    pub role: String,
    pub attempt: u64,
    pub goal: String,
    rework_authorization: Option<Box<ReworkAuthorization>>,
    validation_seal: Sha256Digest,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionIntentSealIdentity<'intent> {
    delivery: &'intent DeliverySnapshot,
    execution_job_id: &'intent ExecutionJobId,
    product_session_id: &'intent ProductSessionId,
    delivery_id: &'intent DeliveryId,
    delivery_task_id: Option<&'intent DeliveryTaskId>,
    stage_run_id: &'intent StageRunId,
    stage: DeliveryStage,
    role: &'intent str,
    attempt: u64,
    goal: &'intent str,
    rework_authorization_digest: Option<&'intent Sha256Digest>,
}

impl ExecutionIntent {
    pub fn rework_authorization(&self) -> Option<&ReworkAuthorization> {
        self.rework_authorization.as_deref()
    }

    /// Confirms this intent is the unchanged output of the Delivery
    /// application service for the exact post-advance snapshot.
    ///
    /// # Errors
    ///
    /// Rejects field mutation, authorization replacement, or pairing the
    /// intent with another Delivery snapshot before durable publication.
    pub fn validate_for_delivery(&self, delivery: &Delivery) -> Result<(), CoordinationError> {
        let expected = seal_execution_intent(delivery.snapshot(), self)?;
        if self.validation_seal == expected {
            Ok(())
        } else {
            Err(CoordinationError::new(
                CoordinationErrorCode::BindingConflict,
                "ExecutionIntent changed after the Delivery stage was prepared",
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageAdvanceEffect {
    Dispatch(ExecutionIntent),
    Review(AttentionItemId),
    Resume(ExecutionIntent),
    Clarify(ReworkClarificationReason),
}

/// Immutable outbox projection for a bounded rework decision that requires
/// human clarification instead of another Worker job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryReworkClarifiedEvent {
    pub schema_version: u8,
    pub delivery_id: DeliveryId,
    pub delivery_revision: u64,
    pub reason: ReworkClarificationReason,
    pub occurred_at_millis: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageAdvanceResult {
    pub delivery: Delivery,
    pub effect: StageAdvanceEffect,
    source_delivery: Delivery,
    sealed_delivery: Delivery,
    sealed_effect: StageAdvanceEffect,
    kind: StageAdvanceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StageAdvanceKind {
    Start,
    Resume,
    Clarify,
}

impl StageAdvanceResult {
    /// Checks that the public projection still matches the application-owned
    /// transition. Callers may inspect the projection, but cannot turn an
    /// edited copy into stage-start authority.
    ///
    /// # Errors
    ///
    /// Returns a conflict when either the Delivery or execution effect was
    /// changed after [`advance`] created this result.
    pub fn validate_projection(&self) -> Result<(), CoordinationError> {
        if self.delivery != self.sealed_delivery || self.effect != self.sealed_effect {
            return Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "stage advance projection differs from its sealed application transition",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_start_source(
        &self,
        current: &Delivery,
    ) -> Result<(), CoordinationError> {
        self.validate_projection()?;
        if self.kind != StageAdvanceKind::Start {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "only a newly selected stage can be committed as stage.started",
            ));
        }
        if self.source_delivery != *current {
            return Err(CoordinationError::new(
                CoordinationErrorCode::RevisionConflict,
                "stage advance source is not the exact current Delivery",
            ));
        }
        if self.delivery.id() != current.id()
            || self.delivery.revision() != current.revision().saturating_add(1)
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "stage advance result is not the next revision of its source Delivery",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_rework_clarification_source(
        &self,
        current: &Delivery,
    ) -> Result<(), CoordinationError> {
        self.validate_projection()?;
        if self.kind != StageAdvanceKind::Clarify
            || !matches!(self.effect, StageAdvanceEffect::Clarify(_))
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "only a derived bounded-rework decision can be committed as rework.clarified",
            ));
        }
        if self.source_delivery != *current {
            return Err(CoordinationError::new(
                CoordinationErrorCode::RevisionConflict,
                "rework clarification source is not the exact current Delivery",
            ));
        }
        if self.delivery.id() != current.id()
            || self.delivery.revision() != current.revision().saturating_add(1)
            || self.delivery.snapshot().status != DeliveryStatus::Clarifying
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "rework clarification is not the next Clarifying revision of its source Delivery",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelIntent {
    pub stage_run_id: StageRunId,
    pub execution_job_id: ExecutionJobId,
    pub attempt: u64,
    pub product_session_id: ProductSessionId,
    pub worker_session_id: WorkerSessionId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelAcknowledgement {
    pub stage_run_id: StageRunId,
    pub execution_job_id: ExecutionJobId,
    pub attempt: u64,
    pub worker_session_id: WorkerSessionId,
}

/// Validates the fixed Delivery-stage actor and `StrongFlow` role policy.
///
/// # Errors
///
/// Returns [`CoordinationErrorCode::InvalidRequest`] when an execution owner
/// does not match the stage policy.
pub fn validate_stage_executor(
    stage: DeliveryStage,
    actor_type: StageRunActorType,
    role: &str,
) -> Result<(), CoordinationError> {
    let valid = match stage {
        DeliveryStage::Clarifying => {
            actor_type == StageRunActorType::Codex && role == "requirements"
        }
        DeliveryStage::Planning => actor_type == StageRunActorType::Codex && role == "planner",
        DeliveryStage::PlanReview => actor_type == StageRunActorType::Human && role == "reviewer",
        DeliveryStage::Executing => actor_type == StageRunActorType::Codex && role == "executor",
        DeliveryStage::Verifying => {
            actor_type == StageRunActorType::Codex
                && matches!(role, "reviewer" | "verifier" | "adversarial-verifier")
        }
        DeliveryStage::Reworking => actor_type == StageRunActorType::Codex && role == "remediator",
        DeliveryStage::DeliveryReview => {
            actor_type == StageRunActorType::Human && role == "approver"
        }
    };
    if valid {
        Ok(())
    } else {
        Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "stage actor or role does not match the fixed Delivery policy",
        ))
    }
}

/// Selects and starts the only legal next Delivery stage.
///
/// The caller supplies fresh identities but cannot supply a stage or attempt.
///
/// # Errors
///
/// Returns a stable coordination error when the revision or Delivery state is
/// not valid for one next stage.
pub fn advance(
    delivery: &Delivery,
    input: AdvanceStageInput,
) -> Result<StageAdvanceResult, CoordinationError> {
    let next = select_next_stage(delivery, &input)?;
    let run = StageRun {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: input.identities.stage_run_id.clone(),
        delivery_id: delivery.id().clone(),
        delivery_task_id: next.delivery_task_id.clone(),
        stage: next.stage,
        actor_type: next.actor_type,
        role: next.role.to_owned(),
        status: if next.actor_type == StageRunActorType::Human {
            StageRunStatus::Waiting
        } else {
            StageRunStatus::Running
        },
        attempt: next.attempt,
        started_at_millis: input.now_millis,
        finished_at_millis: None,
    };
    let mut snapshot = delivery.clone().into_snapshot();
    snapshot.revision += 1;
    snapshot.status = next.next_status;
    settle_previous_run(
        &mut snapshot,
        next.previous,
        input.previous_outcome.as_ref(),
    )?;
    start_selected_task(&mut snapshot, &next)?;
    if next.stage == DeliveryStage::Reworking {
        crate::domain::rework::invalidate_candidate_authorization_for_writer_start(&mut snapshot);
    }
    snapshot.stage_runs.push(run);
    snapshot.updated_at_millis = input.now_millis;
    let effect = append_stage_effect(delivery, &mut snapshot, &next, input)?;
    let advanced_delivery = Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
    })?;
    Ok(StageAdvanceResult {
        source_delivery: delivery.clone(),
        sealed_delivery: advanced_delivery.clone(),
        sealed_effect: effect.clone(),
        delivery: advanced_delivery,
        effect,
        kind: StageAdvanceKind::Start,
    })
}

/// Consumes the sealed precise-rework decision instead of letting a caller
/// choose a task or silently ignore a clarification result.
///
/// A start decision delegates to [`advance`] with the exact authorization. A
/// bounded/repeated failure changes the Delivery to `Clarifying` without
/// creating a remediator `StageRun`, `SessionBinding`, or `ExecutionJob`.
///
/// # Errors
///
/// Rejects a stale revision, non-reworking Delivery, active run, unresolved
/// Attention, or an input that tries to combine another authorization or
/// terminal/review facts with the sealed decision.
pub fn advance_rework(
    delivery: &Delivery,
    mut input: AdvanceStageInput,
    decision: ReworkDecision,
) -> Result<StageAdvanceResult, CoordinationError> {
    if input.rework_authorization.is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "advance_rework owns the sealed authorization input",
        ));
    }
    match decision {
        ReworkDecision::Start(authorization) => {
            input.rework_authorization = Some(authorization);
            advance(delivery, input)
        }
        ReworkDecision::Clarify(clarification) => {
            clarification
                .validate_for_transition(delivery)
                .map_err(|error| {
                    CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
                })?;
            let reason = clarification.reason();
            if delivery.revision() != input.expected_revision {
                return Err(CoordinationError::new(
                    CoordinationErrorCode::RevisionConflict,
                    "Delivery revision changed before rework clarification",
                ));
            }
            require_mutation_time(delivery, input.now_millis)?;
            let invalid_input = input.review.is_some()
                || input.previous_outcome.is_some()
                || input.current_lease.is_some()
                || delivery.snapshot().status != DeliveryStatus::Reworking
                || delivery.snapshot().stage_runs.iter().any(is_active)
                || delivery
                    .snapshot()
                    .attention_items
                    .iter()
                    .any(|item| item.blocking && item.status == AttentionItemStatus::Open);
            if invalid_input {
                return Err(CoordinationError::new(
                    CoordinationErrorCode::WrongState,
                    "rework clarification requires one idle, unblocked Reworking Delivery",
                ));
            }
            let mut snapshot = delivery.clone().into_snapshot();
            snapshot.status = DeliveryStatus::Clarifying;
            snapshot.revision += 1;
            snapshot.updated_at_millis = input.now_millis;
            let clarified_delivery = Delivery::try_from_snapshot(snapshot).map_err(|error| {
                CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
            })?;
            let effect = StageAdvanceEffect::Clarify(reason);
            Ok(StageAdvanceResult {
                source_delivery: delivery.clone(),
                sealed_delivery: clarified_delivery.clone(),
                sealed_effect: effect.clone(),
                delivery: clarified_delivery,
                effect,
                kind: StageAdvanceKind::Clarify,
            })
        }
    }
}

struct NextStage<'delivery> {
    previous: Option<&'delivery StageRun>,
    stage: DeliveryStage,
    next_status: DeliveryStatus,
    actor_type: StageRunActorType,
    delivery_task_id: Option<DeliveryTaskId>,
    role: &'static str,
    attempt: u64,
}

fn select_next_stage<'delivery>(
    delivery: &'delivery Delivery,
    input: &AdvanceStageInput,
) -> Result<NextStage<'delivery>, CoordinationError> {
    if delivery.revision() != input.expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before stage advance",
        ));
    }
    require_mutation_time(delivery, input.now_millis)?;
    if delivery
        .snapshot()
        .attention_items
        .iter()
        .any(|item| item.blocking && item.status == AttentionItemStatus::Open)
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::AttentionRequired,
            "an open blocking AttentionItem must be resolved before stage advance",
        ));
    }
    let mut active = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| is_active(run));
    let previous = active.next();
    if active.next().is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "Delivery has more than one active StageRun",
        ));
    }
    if let Some(previous) = previous {
        validate_stage_executor(previous.stage, previous.actor_type, &previous.role)?;
    }
    let (stage, next_status, actor_type) =
        legal_transition(delivery.snapshot().status, previous.map(|run| run.stage))?;
    validate_previous_outcome(
        delivery,
        previous,
        input.previous_outcome.as_ref(),
        input.current_lease.as_ref(),
        input.now_millis,
    )?;
    let rework_authorization = validate_rework_authorization(delivery, stage, input)?;
    let delivery_task_id = select_task_id(delivery, stage, previous, rework_authorization)?;
    let role = role_for_stage(delivery, stage, previous, delivery_task_id.as_ref())?;
    validate_stage_executor(stage, actor_type, role)?;
    let attempt = rework_authorization.map_or_else(
        || {
            delivery
                .snapshot()
                .stage_runs
                .iter()
                .filter(|run| {
                    run.stage == stage
                        && run.role == role
                        && run.delivery_task_id == delivery_task_id
                })
                .count() as u64
                + 1
        },
        ReworkAuthorization::next_attempt,
    );
    Ok(NextStage {
        previous,
        stage,
        next_status,
        actor_type,
        delivery_task_id,
        role,
        attempt,
    })
}

fn validate_rework_authorization<'input>(
    delivery: &Delivery,
    stage: DeliveryStage,
    input: &'input AdvanceStageInput,
) -> Result<Option<&'input ReworkAuthorization>, CoordinationError> {
    match (stage, input.rework_authorization.as_deref()) {
        (DeliveryStage::Reworking, Some(authorization)) => {
            authorization
                .validate_for_dispatch(delivery)
                .map_err(|error| {
                    CoordinationError::new(
                        CoordinationErrorCode::Conflict,
                        format!("rework dispatch authorization is stale: {error}"),
                    )
                })?;
            Ok(Some(authorization))
        }
        (DeliveryStage::Reworking, None) => Err(CoordinationError::new(
            CoordinationErrorCode::AttentionRequired,
            "rework dispatch requires a sealed current-candidate authorization",
        )),
        (_, Some(_)) => Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "rework authorization cannot be attached to a non-reworking stage",
        )),
        (_, None) => Ok(None),
    }
}

fn validate_previous_outcome(
    delivery: &Delivery,
    previous: Option<&StageRun>,
    outcome: Option<&VerifiedTerminalOutcome>,
    current_lease: Option<&ActiveLeaseIdentity>,
    handoff_at_millis: u64,
) -> Result<(), CoordinationError> {
    let Some(previous) = previous else {
        return if outcome.is_none() && current_lease.is_none() {
            Ok(())
        } else {
            Err(CoordinationError::new(
                CoordinationErrorCode::InvalidRequest,
                "a terminal outcome or lease was supplied without an active StageRun",
            ))
        };
    };
    let binding = exact_binding(delivery, previous, true)?;
    let outcome = outcome.ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "an active StageRun requires a verified terminal Worker outcome before handoff",
        )
    })?;
    let current_lease = current_lease.ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "an active StageRun handoff requires the authoritative current lease identity",
        )
    })?;
    if outcome.lease_identity != *current_lease {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "successful terminal outcome no longer matches the authoritative current lease",
        ));
    }
    let exact = outcome.stage_run_id == previous.id
        && outcome.lease_identity.execution_job_id == binding.execution_job_id
        && binding.worker_session_id.as_ref() == Some(&outcome.lease_identity.worker_session_id)
        && binding.codex_thread_id.as_ref() == outcome.codex_thread_id()
        && outcome.lease_identity.attempt == previous.attempt
        && outcome.finished_at_millis() >= previous.started_at_millis
        && outcome.finished_at_millis() <= handoff_at_millis
        && outcome.status == TerminalOutcomeStatus::Succeeded;
    if exact {
        Ok(())
    } else {
        Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "only the exact matching successful terminal outcome permits stage handoff",
        ))
    }
}

fn settle_previous_run(
    snapshot: &mut DeliverySnapshot,
    previous: Option<&StageRun>,
    outcome: Option<&VerifiedTerminalOutcome>,
) -> Result<(), CoordinationError> {
    if let Some(previous) = previous {
        let finished_at_millis = outcome
            .filter(|outcome| outcome.stage_run_id() == &previous.id)
            .map(VerifiedTerminalOutcome::finished_at_millis)
            .ok_or_else(|| {
                CoordinationError::new(
                    CoordinationErrorCode::WrongState,
                    "the previous StageRun has no exact verified finish time",
                )
            })?;
        let stored = snapshot
            .stage_runs
            .iter_mut()
            .find(|run| run.id == previous.id)
            .ok_or_else(|| {
                CoordinationError::new(
                    CoordinationErrorCode::Conflict,
                    "the active StageRun disappeared while preparing the handoff",
                )
            })?;
        stored.status = StageRunStatus::Succeeded;
        stored.finished_at_millis = Some(finished_at_millis);
    }
    Ok(())
}

fn start_selected_task(
    snapshot: &mut DeliverySnapshot,
    next: &NextStage<'_>,
) -> Result<(), CoordinationError> {
    let Some(task_id) = &next.delivery_task_id else {
        return Ok(());
    };
    let task = snapshot
        .tasks
        .iter_mut()
        .find(|task| &task.id == task_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "selected DeliveryTask disappeared while preparing the stage",
            )
        })?;
    let fact = match next.stage {
        DeliveryStage::Executing => TaskFact::StartExecuting,
        DeliveryStage::Verifying => TaskFact::StartVerifying,
        DeliveryStage::Reworking => TaskFact::StartReworking,
        _ => {
            return Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "a Delivery-level stage unexpectedly selected a task",
            ));
        }
    };
    task.status = transition_task_status(task.status, fact)?;
    Ok(())
}

fn append_stage_effect(
    delivery: &Delivery,
    snapshot: &mut DeliverySnapshot,
    next: &NextStage<'_>,
    input: AdvanceStageInput,
) -> Result<StageAdvanceEffect, CoordinationError> {
    if next.actor_type == StageRunActorType::Human {
        append_review_effect(delivery, snapshot, next.stage, input)
    } else {
        append_execution_effect(delivery, snapshot, next, input)
    }
}

fn append_review_effect(
    delivery: &Delivery,
    snapshot: &mut DeliverySnapshot,
    stage: DeliveryStage,
    input: AdvanceStageInput,
) -> Result<StageAdvanceEffect, CoordinationError> {
    let review = input.review.ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::AttentionRequired,
            "a human review stage requires frozen linked Attention",
        )
    })?;
    let item_type = match stage {
        DeliveryStage::PlanReview => AttentionItemType::DecisionRequired,
        DeliveryStage::DeliveryReview => AttentionItemType::DeliveryApproval,
        _ => {
            return Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "a non-review stage was assigned to a human actor",
            ));
        }
    };
    snapshot.attention_items.push(AttentionItem {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: input.identities.attention_item_id.clone(),
        delivery_id: delivery.id().clone(),
        delivery_spec_id: snapshot.spec.id.clone(),
        stage_run_id: Some(input.identities.stage_run_id),
        item_type,
        title: review.title,
        context: review.context,
        options: Vec::new(),
        assigned_to: Some(review.assigned_to),
        blocking: true,
        status: AttentionItemStatus::Open,
        resolution: None,
        resolved_by: None,
        created_at_millis: input.now_millis,
        resolved_at_millis: None,
    });
    Ok(StageAdvanceEffect::Review(
        input.identities.attention_item_id,
    ))
}

fn append_execution_effect(
    delivery: &Delivery,
    snapshot: &mut DeliverySnapshot,
    next: &NextStage<'_>,
    input: AdvanceStageInput,
) -> Result<StageAdvanceEffect, CoordinationError> {
    if input.review.is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "Codex stages do not create business review Attention",
        ));
    }
    snapshot.session_bindings.push(SessionBinding {
        schema_version: DELIVERY_SCHEMA_VERSION,
        id: input.identities.session_binding_id,
        delivery_id: delivery.id().clone(),
        delivery_task_id: next.delivery_task_id.clone(),
        stage_run_id: input.identities.stage_run_id.clone(),
        product_session_id: input.product_session_id.clone(),
        execution_job_id: input.identities.execution_job_id.clone(),
        worker_session_id: None,
        codex_thread_id: None,
        bound_at_millis: input.now_millis,
    });
    let mut intent = ExecutionIntent {
        execution_job_id: input.identities.execution_job_id,
        product_session_id: input.product_session_id,
        delivery_id: delivery.id().clone(),
        delivery_task_id: next.delivery_task_id.clone(),
        stage_run_id: input.identities.stage_run_id,
        stage: next.stage,
        role: next.role.to_owned(),
        attempt: next.attempt,
        goal: goal_for_task(delivery, next.delivery_task_id.as_ref()),
        rework_authorization: input.rework_authorization,
        validation_seal: Sha256Digest(String::new()),
    };
    intent.validation_seal = seal_execution_intent(snapshot, &intent)?;
    Ok(StageAdvanceEffect::Dispatch(intent))
}

fn seal_execution_intent(
    delivery: &DeliverySnapshot,
    intent: &ExecutionIntent,
) -> Result<Sha256Digest, CoordinationError> {
    let identity = ExecutionIntentSealIdentity {
        delivery,
        execution_job_id: &intent.execution_job_id,
        product_session_id: &intent.product_session_id,
        delivery_id: &intent.delivery_id,
        delivery_task_id: intent.delivery_task_id.as_ref(),
        stage_run_id: &intent.stage_run_id,
        stage: intent.stage,
        role: &intent.role,
        attempt: intent.attempt,
        goal: &intent.goal,
        rework_authorization_digest: intent
            .rework_authorization
            .as_deref()
            .map(ReworkAuthorization::authorization_digest),
    };
    let encoded = serde_json::to_vec(&identity).map_err(|error| {
        CoordinationError::new(
            CoordinationErrorCode::Conflict,
            format!("ExecutionIntent seal cannot be encoded: {error}"),
        )
    })?;
    Ok(Sha256Digest(format!(
        "sha256:{:x}",
        Sha256::digest(encoded)
    )))
}

fn goal_for_task(delivery: &Delivery, task_id: Option<&DeliveryTaskId>) -> String {
    task_id
        .and_then(|task_id| {
            delivery
                .snapshot()
                .tasks
                .iter()
                .find(|task| &task.id == task_id)
        })
        .map_or_else(
            || delivery.snapshot().spec.goal.clone(),
            |task| task.goal.clone(),
        )
}

/// Rebuilds the immutable `ExecutionIntent` for the one active Codex `StageRun`.
///
/// Recovery does not append a run, allocate an attempt, or change Delivery
/// revision. Durable outbox replay may redeliver this same job identity.
///
/// # Errors
///
/// Fails closed on a stale revision, zero/multiple active runs, a human review
/// stage, or a conflicting exact `SessionBinding`. A pending dispatch may not
/// have a `WorkerSession` or `CodexThread` yet; durable replay must still reuse
/// its original job and attempt.
pub fn resume_active(
    delivery: &Delivery,
    expected_revision: u64,
) -> Result<StageAdvanceResult, CoordinationError> {
    if delivery.revision() != expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before stage resume",
        ));
    }
    let mut active = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| is_active(run));
    let run = active.next().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "Delivery has no active Codex StageRun to resume",
        )
    })?;
    if active.next().is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "Delivery has more than one active StageRun",
        ));
    }
    if run.stage == DeliveryStage::Reworking {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "a remediator dispatch must replay its committed authorized ExecutionJob instead of reconstructing an unscoped intent",
        ));
    }
    let binding = exact_binding(delivery, run, false)?;
    let goal = run
        .delivery_task_id
        .as_ref()
        .and_then(|task_id| {
            delivery
                .snapshot()
                .tasks
                .iter()
                .find(|task| &task.id == task_id)
        })
        .map_or_else(
            || delivery.snapshot().spec.goal.clone(),
            |task| task.goal.clone(),
        );
    let mut intent = ExecutionIntent {
        execution_job_id: binding.execution_job_id.clone(),
        product_session_id: binding.product_session_id.clone(),
        delivery_id: run.delivery_id.clone(),
        delivery_task_id: run.delivery_task_id.clone(),
        stage_run_id: run.id.clone(),
        stage: run.stage,
        role: run.role.clone(),
        attempt: run.attempt,
        goal,
        rework_authorization: None,
        validation_seal: Sha256Digest(String::new()),
    };
    intent.validation_seal = seal_execution_intent(delivery.snapshot(), &intent)?;
    let effect = StageAdvanceEffect::Resume(intent);
    Ok(StageAdvanceResult {
        delivery: delivery.clone(),
        effect: effect.clone(),
        source_delivery: delivery.clone(),
        sealed_delivery: delivery.clone(),
        sealed_effect: effect,
        kind: StageAdvanceKind::Resume,
    })
}

/// Creates the durable cancellation intent for the current exact job.
///
/// The returned value is a pending effect: a Control Plane transaction must
/// commit it to the outbox before any `ExecutionPort` adapter sends it.
///
/// # Errors
///
/// Fails closed on stale revision, ambiguous active state, or incomplete
/// `SessionBinding`.
pub fn request_cancel(
    delivery: &Delivery,
    expected_revision: u64,
) -> Result<CancelIntent, CoordinationError> {
    if delivery.revision() != expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before cancellation",
        ));
    }
    let mut active = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| is_active(run));
    let run = active.next().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "Delivery has no active Codex StageRun to cancel",
        )
    })?;
    if active.next().is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::Conflict,
            "Delivery has more than one active StageRun",
        ));
    }
    let binding = exact_binding(delivery, run, true)?;
    let worker_session_id = binding.worker_session_id.clone().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "the active job has no accepted WorkerSession",
        )
    })?;
    Ok(CancelIntent {
        stage_run_id: run.id.clone(),
        execution_job_id: binding.execution_job_id.clone(),
        attempt: run.attempt,
        product_session_id: binding.product_session_id.clone(),
        worker_session_id,
    })
}

/// Validates a Worker cancellation acknowledgement without settling Delivery.
///
/// `job.cancel_ack` only proves receipt of the request. The returned Delivery
/// is byte-for-byte unchanged; only a verified terminal `job.outcome` may end
/// the `StageRun`.
///
/// # Errors
///
/// Fails closed when the acknowledgement identifies another run or job.
pub fn acknowledge_cancel(
    delivery: &Delivery,
    intent: &CancelIntent,
    acknowledgement: &CancelAcknowledgement,
) -> Result<Delivery, CoordinationError> {
    if acknowledgement.stage_run_id != intent.stage_run_id
        || acknowledgement.execution_job_id != intent.execution_job_id
        || acknowledgement.attempt != intent.attempt
        || acknowledgement.worker_session_id != intent.worker_session_id
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "cancellation acknowledgement does not match the requested job",
        ));
    }
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == intent.stage_run_id && is_active(run))
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "cancellation acknowledgement does not match an active StageRun",
            )
        })?;
    let binding = exact_binding(delivery, run, true)?;
    if binding.execution_job_id != intent.execution_job_id
        || binding.worker_session_id.as_ref() != Some(&intent.worker_session_id)
        || run.attempt != intent.attempt
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "cancellation request no longer matches the active binding",
        ));
    }
    Ok(delivery.clone())
}

/// Applies one verified terminal Worker outcome to its still-current lease.
///
/// Verification and application are separate durable steps. The scheduler
/// identity is therefore checked again here so a result verified before a
/// re-lease cannot settle the newly leased attempt. A Worker process result
/// never advances Delivery by itself; failed, infrastructure-error, and
/// cancelled results leave the Delivery in its current retry phase.
///
/// # Errors
///
/// Fails closed on stale revision, changed lease/fencing/Worker identity,
/// changed active binding, invalid finish time, or an incompatible task state.
pub fn apply_terminal_outcome(
    delivery: &Delivery,
    expected_revision: u64,
    active_lease: &ActiveLeaseIdentity,
    outcome: &VerifiedTerminalOutcome,
) -> Result<Delivery, CoordinationError> {
    if delivery.revision() != expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before terminal outcome",
        ));
    }
    require_mutation_time(delivery, outcome.finished_at_millis())?;
    if &outcome.lease_identity != active_lease {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "terminal outcome was verified for another active lease",
        ));
    }
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == outcome.stage_run_id && is_active(run))
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "terminal outcome does not match the active StageRun",
            )
        })?;
    if outcome.status == TerminalOutcomeStatus::Succeeded
        && (run.stage != DeliveryStage::Verifying
            || !matches!(run.role.as_str(), "verifier" | "adversarial-verifier"))
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "ordinary successful outcomes settle only in the atomic next-stage handoff",
        ));
    }
    let binding = exact_binding(delivery, run, false)?;
    if run.attempt != outcome.lease_identity.attempt
        || binding.execution_job_id != outcome.lease_identity.execution_job_id
        || binding.worker_session_id.as_ref() != Some(&outcome.lease_identity.worker_session_id)
        || binding.codex_thread_id.as_ref() != outcome.codex_thread_id()
        || outcome.finished_at_millis() < run.started_at_millis
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "terminal outcome no longer matches the active StageRun, job, and WorkerSession",
        ));
    }

    let run_id = run.id.clone();
    let run_stage = run.stage;
    let task_id = run.delivery_task_id.clone();
    let mut snapshot = delivery.clone().into_snapshot();
    let stored_run = snapshot
        .stage_runs
        .iter_mut()
        .find(|stored| stored.id == run_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "the active StageRun disappeared while applying terminal outcome",
            )
        })?;
    stored_run.status = match outcome.status {
        TerminalOutcomeStatus::Succeeded => StageRunStatus::Succeeded,
        TerminalOutcomeStatus::Failed | TerminalOutcomeStatus::InfrastructureError => {
            StageRunStatus::Failed
        }
        TerminalOutcomeStatus::Cancelled => StageRunStatus::Cancelled,
    };
    stored_run.finished_at_millis = Some(outcome.finished_at_millis());
    if outcome.status != TerminalOutcomeStatus::Succeeded
        && let Some(task_id) = task_id
    {
        restore_task_after_unsuccessful_outcome(&mut snapshot, &task_id, run_stage)?;
    }
    snapshot.revision += 1;
    snapshot.updated_at_millis = outcome.finished_at_millis();
    Delivery::try_from_snapshot(snapshot)
        .map_err(|error| CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string()))
}

fn restore_task_after_unsuccessful_outcome(
    snapshot: &mut DeliverySnapshot,
    task_id: &DeliveryTaskId,
    run_stage: DeliveryStage,
) -> Result<(), CoordinationError> {
    let task = snapshot
        .tasks
        .iter_mut()
        .find(|task| &task.id == task_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "terminal StageRun task disappeared",
            )
        })?;
    let (expected, next) = match run_stage {
        DeliveryStage::Executing => (DeliveryTaskStatus::Active, DeliveryTaskStatus::Pending),
        DeliveryStage::Verifying => (DeliveryTaskStatus::Verifying, DeliveryTaskStatus::Verifying),
        DeliveryStage::Reworking => (DeliveryTaskStatus::Active, DeliveryTaskStatus::Failed),
        _ => {
            return Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "a Delivery-level StageRun unexpectedly targeted a task",
            ));
        }
    };
    if task.status != expected {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "DeliveryTask changed before terminal outcome",
        ));
    }
    task.status = next;
    Ok(())
}

fn legal_transition(
    delivery_status: DeliveryStatus,
    active_stage: Option<DeliveryStage>,
) -> Result<(DeliveryStage, DeliveryStatus, StageRunActorType), CoordinationError> {
    let transition = match (delivery_status, active_stage) {
        (DeliveryStatus::Draft | DeliveryStatus::Clarifying, None) => (
            DeliveryStage::Clarifying,
            DeliveryStatus::Clarifying,
            StageRunActorType::Codex,
        ),
        (DeliveryStatus::Ready | DeliveryStatus::Planning, None) => (
            DeliveryStage::Planning,
            DeliveryStatus::Planning,
            StageRunActorType::Codex,
        ),
        (DeliveryStatus::Planning, Some(DeliveryStage::Planning)) => (
            DeliveryStage::PlanReview,
            DeliveryStatus::NeedsAttention,
            StageRunActorType::Human,
        ),
        (DeliveryStatus::Executing, None) => (
            DeliveryStage::Executing,
            DeliveryStatus::Executing,
            StageRunActorType::Codex,
        ),
        (DeliveryStatus::Executing, Some(DeliveryStage::Executing))
        | (DeliveryStatus::Verifying, None | Some(DeliveryStage::Verifying))
        | (DeliveryStatus::Reworking, Some(DeliveryStage::Reworking)) => (
            DeliveryStage::Verifying,
            DeliveryStatus::Verifying,
            StageRunActorType::Codex,
        ),
        (DeliveryStatus::Reworking, None) => (
            DeliveryStage::Reworking,
            DeliveryStatus::Reworking,
            StageRunActorType::Codex,
        ),
        (DeliveryStatus::ReadyToDeliver, None) => (
            DeliveryStage::DeliveryReview,
            DeliveryStatus::NeedsAttention,
            StageRunActorType::Human,
        ),
        _ => {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "Delivery status and active StageRun do not have one legal next stage",
            ));
        }
    };
    Ok(transition)
}

fn select_task_id(
    delivery: &Delivery,
    stage: DeliveryStage,
    previous: Option<&StageRun>,
    rework_authorization: Option<&ReworkAuthorization>,
) -> Result<Option<DeliveryTaskId>, CoordinationError> {
    if !matches!(
        stage,
        DeliveryStage::Executing | DeliveryStage::Verifying | DeliveryStage::Reworking
    ) {
        return Ok(None);
    }
    if delivery.snapshot().tasks.is_empty() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "execution requires a non-empty approved DeliveryTask graph",
        ));
    }
    if stage == DeliveryStage::Verifying
        && let Some(previous) = previous
    {
        return previous.delivery_task_id.clone().map(Some).ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "task verification cannot follow a Delivery-level writer",
            )
        });
    }
    if stage == DeliveryStage::Reworking {
        return rework_authorization
            .map(|authorization| Some(authorization.delivery_task_id().clone()))
            .ok_or_else(|| {
                CoordinationError::new(
                    CoordinationErrorCode::AttentionRequired,
                    "rework task selection requires the sealed authorization",
                )
            });
    }
    Ok(Some(runnable_task(delivery, stage)?.id.clone()))
}

fn role_for_stage(
    delivery: &Delivery,
    stage: DeliveryStage,
    previous: Option<&StageRun>,
    delivery_task_id: Option<&DeliveryTaskId>,
) -> Result<&'static str, CoordinationError> {
    let fixed = match stage {
        DeliveryStage::Clarifying => Some("requirements"),
        DeliveryStage::Planning => Some("planner"),
        DeliveryStage::PlanReview => Some("reviewer"),
        DeliveryStage::Executing => Some("executor"),
        DeliveryStage::Reworking => Some("remediator"),
        DeliveryStage::DeliveryReview => Some("approver"),
        DeliveryStage::Verifying => None,
    };
    if let Some(role) = fixed {
        return Ok(role);
    }
    if let Some(previous) = previous {
        return match (previous.stage, previous.role.as_str()) {
            (DeliveryStage::Executing | DeliveryStage::Reworking, _) => Ok("reviewer"),
            (DeliveryStage::Verifying, "reviewer") => Ok("verifier"),
            (DeliveryStage::Verifying, "verifier" | "adversarial-verifier") => {
                Err(CoordinationError::new(
                    CoordinationErrorCode::WrongState,
                    "all required verification roles completed; submit a DeliveryVerdict",
                ))
            }
            _ => Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "verification progress contains an unexpected role",
            )),
        };
    }
    let last_writer_index = delivery.snapshot().stage_runs.iter().rposition(|run| {
        matches!(
            run.stage,
            DeliveryStage::Executing | DeliveryStage::Reworking
        ) && run.delivery_task_id.as_ref() == delivery_task_id
    });
    let completed_roles = delivery
        .snapshot()
        .stage_runs
        .iter()
        .enumerate()
        .filter(|(index, run)| {
            last_writer_index.is_none_or(|writer| *index > writer)
                && run.stage == DeliveryStage::Verifying
                && run.delivery_task_id.as_ref() == delivery_task_id
                && run.status == StageRunStatus::Succeeded
        })
        .map(|(_, run)| run.role.as_str())
        .collect::<Vec<_>>();
    ["reviewer", "verifier"]
        .into_iter()
        .find(|role| !completed_roles.contains(role))
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "all required verification roles completed; submit a DeliveryVerdict",
            )
        })
}

fn exact_binding<'delivery>(
    delivery: &'delivery Delivery,
    run: &StageRun,
    require_worker_session: bool,
) -> Result<&'delivery SessionBinding, CoordinationError> {
    validate_stage_executor(run.stage, run.actor_type, &run.role)?;
    if run.actor_type == StageRunActorType::Human {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "human review stages are not ExecutionJob SessionBindings",
        ));
    }
    let mut bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == run.id);
    let binding = bindings.next().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "the active Codex StageRun has no exact SessionBinding",
        )
    })?;
    if bindings.next().is_some()
        || binding.delivery_id != run.delivery_id
        || binding.delivery_task_id != run.delivery_task_id
        || (require_worker_session && binding.worker_session_id.is_none())
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "the active Codex StageRun SessionBinding is incomplete or conflicting",
        ));
    }
    Ok(binding)
}

fn is_active(run: &StageRun) -> bool {
    matches!(
        run.status,
        StageRunStatus::Running | StageRunStatus::Waiting
    )
}
