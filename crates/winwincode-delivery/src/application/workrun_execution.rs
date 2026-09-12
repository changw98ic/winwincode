// SPDX-License-Identifier: Apache-2.0

//! Delivery `WorkRun` execution coordination service.
//!
//! Raw Scheduler and Worker facts are deliberately not an external production
//! API while their authoritative Phase 4 adapters do not exist.
//!
//! ```compile_fail
//! use winwincode_delivery::application::workrun_execution::ActiveLeaseIdentity;
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
//! use winwincode_delivery::application::workrun_execution::SessionBindingAuthority;
//!
//! let _caller_built_authority = SessionBindingAuthority {
//!     active_lease: todo!(),
//!     issued_at: todo!(),
//!     expires_at: todo!(),
//! };
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::workrun_execution::SessionBindingAuthority;
//!
//! let _deserialized: SessionBindingAuthority = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::workrun_execution::TerminalWorkerOutcome;
//!
//! let _caller_built_outcome = TerminalWorkerOutcome {
//!     work_run_id: todo!(),
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
//!     winwincode_delivery::application::workrun_execution::verify_terminal_outcome;
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::workrun_execution::DeliveryTerminalOutcomeFacts;
//!
//! let _caller_built_facts = DeliveryTerminalOutcomeFacts {
//!     authority: todo!(),
//!     outcome: todo!(),
//! };
//! ```
//!
//! ```compile_fail
//! use winwincode_delivery::application::workrun_execution::DeliveryTerminalOutcomeFacts;
//!
//! let _deserialized: DeliveryTerminalOutcomeFacts = serde_json::from_str("{}").unwrap();
//! ```

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use winwincode_domain::{
    ArtifactId, CodexThreadId, DeliveryId, ExecutionAckSequence, ExecutionJobId, FencingToken,
    Instant, LeaseId, ProductSessionId, Revision, Sha256Digest, WorkContractId, WorkItemId,
    WorkRun, WorkRunId, WorkRunState, WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_storage::ExecutionDispatchAuthority;

use crate::domain::{
    AttentionItemStatus, Delivery, DeliverySnapshot, DeliveryStatus, SessionBinding,
    rework::{ReworkAuthorization, ReworkClarificationReason, ReworkDecision},
};
use crate::domain::{MAX_COLLECTION_LENGTH, MAX_SAFE_INTEGER};

use super::workrun::WorkRunAggregate;
use super::{CoordinationError, CoordinationErrorCode, require_mutation_time};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkRunIdentities {
    pub work_contract_id: WorkContractId,
    pub work_contract_revision: Revision,
    pub work_item_id: WorkItemId,
    pub work_item_revision: Revision,
    pub work_run_id: WorkRunId,
    pub execution_job_id: ExecutionJobId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewAttentionSeed {
    pub title: String,
    pub context: String,
    pub assigned_to: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReworkAdvanceInput {
    pub expected_revision: u64,
    pub product_session_id: ProductSessionId,
    pub identities: NewWorkRunIdentities,
    pub review: Option<ReviewAttentionSeed>,
    pub previous_outcome: Option<VerifiedTerminalOutcome>,
    pub current_lease: Option<ActiveLeaseIdentity>,
    /// Exact current-candidate remediation authority. Required only for a
    /// `Reworking` `WorkItem` and consumed into the immutable dispatch intent.
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
    #[must_use]
    pub fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    #[must_use]
    pub const fn attempt(&self) -> u64 {
        self.attempt
    }

    #[must_use]
    pub fn lease_id(&self) -> &LeaseId {
        &self.lease_id
    }

    #[must_use]
    pub fn fencing_token(&self) -> &FencingToken {
        &self.fencing_token
    }

    #[must_use]
    pub fn worker_id(&self) -> &WorkerId {
        &self.worker_id
    }

    #[must_use]
    pub fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.worker_instance_id
    }

    #[must_use]
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

/// Seals the Registry's accepted dispatch record for Delivery/Runtime ingress.
///
/// [`ExecutionDispatchAuthority`] has no public field constructor and is
/// returned only after the durable Registry has joined an accepted Worker
/// dispatch result to its exact current lease. Keeping this conversion here
/// prevents Worker message fields from becoming scheduler authority.
#[must_use]
pub fn seal_session_binding_authority(
    dispatch: &ExecutionDispatchAuthority,
) -> SessionBindingAuthority {
    let lease = dispatch.lease();
    SessionBindingAuthority {
        active_lease: ActiveLeaseIdentity {
            execution_job_id: lease.job_id.clone(),
            attempt: lease.attempt,
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            worker_session_id: dispatch.worker_session_id().clone(),
        },
        issued_at: lease.issued_at.clone(),
        expires_at: lease.expires_at.clone(),
    }
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

/// Exact scheduler lease and accepted Worker outcome read by a trusted
/// production adapter from durable state.
///
/// This input is not itself authority. [`reconcile_durable_terminal_outcome`]
/// checks every field against the current Delivery `WorkRun` and
/// `SessionBinding` before sealing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableTerminalOutcomeInput {
    pub execution_job_id: ExecutionJobId,
    pub attempt: u64,
    pub lease_id: LeaseId,
    pub fencing_token: FencingToken,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: WorkerSessionId,
    pub issued_at: Instant,
    pub expires_at: Instant,
    pub work_run_id: WorkRunId,
    pub status: TerminalOutcomeStatus,
    pub codex_thread_id: Option<CodexThreadId>,
    pub finished_at_millis: u64,
    pub last_event_sequence: ExecutionAckSequence,
    pub artifacts: Vec<TerminalArtifactReference>,
}

/// Worker-owned terminal values paired with Registry-owned dispatch authority.
///
/// This report deliberately excludes lease, fencing, Worker, and
/// `WorkerSession` fields. Those values always come from the opaque accepted
/// dispatch record when [`seal_dispatch_terminal_outcome`] is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerTerminalOutcomeReport {
    pub work_run_id: WorkRunId,
    pub status: TerminalOutcomeStatus,
    pub codex_thread_id: Option<CodexThreadId>,
    pub finished_at_millis: u64,
    pub last_event_sequence: ExecutionAckSequence,
    pub artifacts: Vec<TerminalArtifactReference>,
}

/// Combines an accepted Registry dispatch with Worker-owned terminal values.
///
/// The returned facts still undergo the canonical current-Delivery and
/// durable-Job join in the Control Plane terminal transaction. This first seal
/// ensures a Worker frame cannot choose its own lease, fence, process, or
/// session authority, while an exact terminal receipt can be replayed after
/// the Delivery has already advanced.
#[must_use]
pub fn seal_dispatch_terminal_outcome(
    dispatch: &ExecutionDispatchAuthority,
    report: WorkerTerminalOutcomeReport,
) -> DeliveryTerminalOutcomeFacts {
    let lease = dispatch.lease();
    terminal_outcome_facts(DurableTerminalOutcomeInput {
        execution_job_id: lease.job_id.clone(),
        attempt: lease.attempt,
        lease_id: lease.lease_id.clone(),
        fencing_token: lease.fencing_token.clone(),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: dispatch.worker_session_id().clone(),
        issued_at: lease.issued_at.clone(),
        expires_at: lease.expires_at.clone(),
        work_run_id: report.work_run_id,
        status: report.status,
        codex_thread_id: report.codex_thread_id,
        finished_at_millis: report.finished_at_millis,
        last_event_sequence: report.last_event_sequence,
        artifacts: report.artifacts,
    })
}

impl DeliveryTerminalOutcomeFacts {
    #[must_use]
    pub const fn authority(&self) -> &SessionBindingAuthority {
        &self.authority
    }

    #[must_use]
    pub const fn work_run_id(&self) -> &WorkRunId {
        &self.outcome.work_run_id
    }

    #[must_use]
    pub const fn status(&self) -> TerminalOutcomeStatus {
        self.outcome.status
    }

    #[must_use]
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

    /// Revalidates the sealed facts for an active-stage handoff.
    ///
    /// # Errors
    ///
    /// Rejects a stale Delivery, binding, Worker, lease, attempt, fencing
    /// token, terminal position, or Artifact reference.
    pub fn verify_active(
        &self,
        delivery: &Delivery,
    ) -> Result<VerifiedTerminalOutcome, CoordinationError> {
        self.verify(delivery)
    }

    /// Revalidates a successful report for read-only candidate derivation,
    /// without requiring the Controller to have settled the `WorkItem` yet.
    pub(crate) fn verify_successful_report(
        &self,
        delivery: &Delivery,
    ) -> Result<VerifiedTerminalOutcome, CoordinationError> {
        if self.outcome.status != TerminalOutcomeStatus::Succeeded {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "candidate source requires a successful terminal outcome",
            ));
        }
        let running = delivery
            .snapshot()
            .work_run_aggregate
            .runs
            .iter()
            .any(|run| run.id == self.outcome.work_run_id && run.state == WorkRunState::Running);
        if running {
            self.verify_active(delivery)
        } else {
            self.verify_settled_success(delivery)
        }
    }

    /// Revalidates one already-settled successful Worker outcome for derived
    /// candidate/source reads. This does not mutate or re-settle Delivery.
    pub(crate) fn verify_settled_success(
        &self,
        delivery: &Delivery,
    ) -> Result<VerifiedTerminalOutcome, CoordinationError> {
        if self.outcome.status != TerminalOutcomeStatus::Succeeded {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "candidate source requires a successful terminal outcome",
            ));
        }
        verify_work_run_terminal(
            delivery,
            self.authority.active_lease(),
            self.outcome.clone(),
            true,
        )
    }
}

/// Reconciles durable scheduler and Worker records with the exact current
/// Delivery before creating terminal authority.
///
/// # Errors
///
/// Fails closed when any input differs from the active `WorkRun` and its
/// complete `SessionBinding`, or when the terminal metadata is invalid.
pub fn reconcile_durable_terminal_outcome(
    delivery: &Delivery,
    input: DurableTerminalOutcomeInput,
) -> Result<DeliveryTerminalOutcomeFacts, CoordinationError> {
    let facts = terminal_outcome_facts(input);
    facts.verify(delivery)?;
    Ok(facts)
}

/// Reconciles a persisted successful outcome after its `WorkRun` was settled by
/// the same atomic handoff transaction.
///
/// # Errors
///
/// Fails closed when the successful `WorkRun`, binding, lease identity,
/// terminal metadata, or finish time no longer matches the Delivery.
pub fn reconcile_durable_settled_terminal_outcome(
    delivery: &Delivery,
    input: DurableTerminalOutcomeInput,
) -> Result<DeliveryTerminalOutcomeFacts, CoordinationError> {
    let facts = terminal_outcome_facts(input);
    facts.verify_settled_success(delivery)?;
    Ok(facts)
}

fn terminal_outcome_facts(input: DurableTerminalOutcomeInput) -> DeliveryTerminalOutcomeFacts {
    let active_lease = ActiveLeaseIdentity {
        execution_job_id: input.execution_job_id.clone(),
        attempt: input.attempt,
        lease_id: input.lease_id.clone(),
        fencing_token: input.fencing_token.clone(),
        worker_id: input.worker_id.clone(),
        worker_instance_id: input.worker_instance_id.clone(),
        worker_session_id: input.worker_session_id.clone(),
    };
    let authority = SessionBindingAuthority {
        active_lease,
        issued_at: input.issued_at,
        expires_at: input.expires_at,
    };
    let outcome = TerminalWorkerOutcome {
        work_run_id: input.work_run_id,
        execution_job_id: input.execution_job_id,
        attempt: input.attempt,
        lease_id: input.lease_id,
        fencing_token: input.fencing_token,
        worker_id: input.worker_id,
        worker_instance_id: input.worker_instance_id,
        worker_session_id: input.worker_session_id,
        status: input.status,
        metadata: TerminalOutcomeMetadata {
            codex_thread_id: input.codex_thread_id,
            finished_at_millis: input.finished_at_millis,
            last_event_sequence: input.last_event_sequence,
            artifacts: input.artifacts,
        },
    };
    DeliveryTerminalOutcomeFacts { authority, outcome }
}

impl SessionBindingAuthority {
    #[must_use]
    pub const fn active_lease(&self) -> &ActiveLeaseIdentity {
        &self.active_lease
    }

    #[must_use]
    pub const fn issued_at(&self) -> &Instant {
        &self.issued_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> &Instant {
        &self.expires_at
    }
}

/// Terminal fact reported by Worker through `ExecutionPort`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalWorkerOutcome {
    work_run_id: WorkRunId,
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
    #[must_use]
    pub fn codex_thread_id(&self) -> Option<&CodexThreadId> {
        self.codex_thread_id.as_ref()
    }

    #[must_use]
    pub const fn finished_at_millis(&self) -> u64 {
        self.finished_at_millis
    }

    #[must_use]
    pub fn last_event_sequence(&self) -> &ExecutionAckSequence {
        &self.last_event_sequence
    }

    #[must_use]
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

/// A terminal fact that matched the current `WorkRun`, `SessionBinding`, and
/// scheduler lease/fencing identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTerminalOutcome {
    work_run_id: WorkRunId,
    lease_identity: ActiveLeaseIdentity,
    status: TerminalOutcomeStatus,
    metadata: TerminalOutcomeMetadata,
}

impl VerifiedTerminalOutcome {
    #[must_use]
    pub fn work_run_id(&self) -> &WorkRunId {
        &self.work_run_id
    }

    #[must_use]
    pub fn execution_job_id(&self) -> &ExecutionJobId {
        &self.lease_identity.execution_job_id
    }

    #[must_use]
    pub fn worker_session_id(&self) -> &WorkerSessionId {
        &self.lease_identity.worker_session_id
    }

    #[must_use]
    pub fn lease_id(&self) -> &LeaseId {
        &self.lease_identity.lease_id
    }

    #[must_use]
    pub fn fencing_token(&self) -> &FencingToken {
        &self.lease_identity.fencing_token
    }

    #[must_use]
    pub fn worker_id(&self) -> &WorkerId {
        &self.lease_identity.worker_id
    }

    #[must_use]
    pub fn worker_instance_id(&self) -> &WorkerInstanceId {
        &self.lease_identity.worker_instance_id
    }

    #[must_use]
    pub const fn attempt(&self) -> u64 {
        self.lease_identity.attempt
    }

    #[must_use]
    pub const fn status(&self) -> TerminalOutcomeStatus {
        self.status
    }

    #[must_use]
    pub fn codex_thread_id(&self) -> Option<&CodexThreadId> {
        self.metadata.codex_thread_id.as_ref()
    }

    #[must_use]
    pub const fn finished_at_millis(&self) -> u64 {
        self.metadata.finished_at_millis
    }

    #[must_use]
    pub fn last_event_sequence(&self) -> &ExecutionAckSequence {
        &self.metadata.last_event_sequence
    }

    #[must_use]
    pub fn artifacts(&self) -> &[TerminalArtifactReference] {
        &self.metadata.artifacts
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn fixture_verified_terminal_outcome(
    work_run_id: WorkRunId,
    lease_identity: ActiveLeaseIdentity,
    status: TerminalOutcomeStatus,
    metadata: TerminalOutcomeMetadata,
) -> VerifiedTerminalOutcome {
    VerifiedTerminalOutcome {
        work_run_id,
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
    verify_work_run_terminal(delivery, lease, outcome, false)
}

fn verify_work_run_terminal(
    delivery: &Delivery,
    lease: &ActiveLeaseIdentity,
    outcome: TerminalWorkerOutcome,
    settled: bool,
) -> Result<VerifiedTerminalOutcome, CoordinationError> {
    use winwincode_domain::WorkRunState;
    let snapshot = delivery.snapshot();
    snapshot.work_run_aggregate.validate().map_err(|_| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "invalid WorkRun aggregate",
        )
    })?;
    let run = snapshot
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.id == outcome.work_run_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "terminal outcome WorkRun is missing",
            )
        })?;
    if if settled {
        !matches!(
            run.state,
            WorkRunState::CandidateReady | WorkRunState::Settled
        )
    } else {
        run.state != WorkRunState::Running
    } {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "terminal outcome WorkRun has the wrong state",
        ));
    }
    let mut bindings = snapshot
        .session_bindings
        .iter()
        .filter(|binding| binding.work_run_id == run.id);
    let binding = bindings.next().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "terminal WorkRun binding is missing",
        )
    })?;
    let exact = bindings.next().is_none()
        && binding.delivery_id == snapshot.id
        && binding.work_contract_id == run.work_contract_id
        && binding.work_contract_revision == run.contract_revision
        && binding.work_item_id == run.work_item_id
        && binding.work_item_revision == run.work_item_revision
        && Some(&binding.product_session_id) == run.product_session_id.as_ref()
        && binding.execution_job_id == run.execution_job_id
        && binding.attempt == lease.attempt
        && binding.worker_id.as_ref() == Some(&run.worker_id)
        && binding.worker_instance_id.as_ref() == Some(&run.worker_instance_id)
        && binding.worker_session_id.as_ref() == Some(&run.worker_session_id)
        && binding.lease_id.as_ref() == Some(&run.lease_id)
        && binding.fencing_token.as_ref().map(|token| &token.0) == Some(&run.fencing_token)
        && binding.codex_thread_id == run.codex_thread_id
        && run.execution_job_id == lease.execution_job_id
        && u64::try_from(run.attempt).ok() == Some(lease.attempt)
        && run.worker_id == lease.worker_id
        && run.worker_instance_id == lease.worker_instance_id
        && run.worker_session_id == lease.worker_session_id
        && run.lease_id == lease.lease_id
        && run.fencing_token == lease.fencing_token.0
        && outcome.execution_job_id == lease.execution_job_id
        && outcome.attempt == lease.attempt
        && outcome.worker_id == lease.worker_id
        && outcome.worker_instance_id == lease.worker_instance_id
        && outcome.worker_session_id == lease.worker_session_id
        && outcome.lease_id == lease.lease_id
        && outcome.fencing_token == lease.fencing_token;
    if !exact {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "terminal outcome does not match exact WorkRun, binding, and lease",
        ));
    }
    validate_terminal_metadata(binding, &outcome.metadata)?;
    Ok(VerifiedTerminalOutcome {
        work_run_id: outcome.work_run_id,
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
        LeaseId, SessionBindingAuthority, Sha256Digest, TerminalArtifactReference,
        TerminalOutcomeMetadata, TerminalOutcomeStatus, TerminalWorkerOutcome,
        VerifiedTerminalOutcome, WorkRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
    };

    #[allow(clippy::too_many_arguments)]
    #[must_use]
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
    #[must_use]
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
    #[must_use]
    pub fn delivery_terminal_outcome_facts(
        authority: SessionBindingAuthority,
        outcome: TerminalWorkerOutcome,
    ) -> DeliveryTerminalOutcomeFacts {
        DeliveryTerminalOutcomeFacts { authority, outcome }
    }

    #[must_use]
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
    #[must_use]
    pub fn terminal_worker_outcome(
        work_run_id: WorkRunId,
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
            work_run_id,
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

    #[must_use]
    pub fn terminal_metadata(outcome: &TerminalWorkerOutcome) -> &TerminalOutcomeMetadata {
        &outcome.metadata
    }
}

fn validate_terminal_metadata(
    binding: &SessionBinding,
    metadata: &TerminalOutcomeMetadata,
) -> Result<(), CoordinationError> {
    let max_sequence = i64::try_from(MAX_SAFE_INTEGER).unwrap_or(i64::MAX);
    if metadata.finished_at_millis > MAX_SAFE_INTEGER
        || metadata.finished_at_millis < binding.bound_at_millis
        || !(0..=max_sequence).contains(&metadata.last_event_sequence.0)
        || metadata.codex_thread_id != binding.codex_thread_id
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "terminal Worker metadata does not match the WorkRun time, CodexThread, or event sequence",
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
    pub work_contract_id: WorkContractId,
    pub work_contract_revision: Revision,
    pub work_item_id: WorkItemId,
    pub work_item_revision: Revision,
    pub work_run_id: WorkRunId,
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
    work_contract_id: &'intent WorkContractId,
    work_contract_revision: &'intent Revision,
    work_item_id: &'intent WorkItemId,
    work_item_revision: &'intent Revision,
    work_run_id: &'intent WorkRunId,
    role: &'intent str,
    attempt: u64,
    goal: &'intent str,
    rework_authorization_digest: Option<&'intent Sha256Digest>,
}

impl ExecutionIntent {
    #[must_use]
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
pub enum WorkRunStartEffect {
    Dispatch(Box<ExecutionIntent>),
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
pub struct WorkRunStartResult {
    pub delivery: Delivery,
    pub effect: WorkRunStartEffect,
    source_delivery: Delivery,
    sealed_delivery: Delivery,
    sealed_effect: WorkRunStartEffect,
    kind: WorkRunStartKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkRunStartKind {
    Clarify,
    WorkRunDispatch,
}

impl WorkRunStartResult {
    /// Creates the canonical `WorkRun` dispatch seal without consulting the
    /// Delivery-wide execution state. This only seals one current `WorkRun` dispatch.
    /// accepted input. Sealed rework retires its candidate producer and reopens
    /// that item. `WorkRun` and `SessionBinding` are added only after scheduler
    /// acceptance. Authorized rework invalidates the previous Evidence/Verdict
    /// atomically with the new immutable dispatch intent.
    ///
    /// # Errors
    ///
    /// Rejects stale revisions, non-runnable items, invalid identities, or a
    /// mutation timestamp older than the current Delivery.
    #[allow(clippy::too_many_arguments)]
    pub fn canonical_workrun_dispatch(
        source: &Delivery,
        rework_authorization: Option<Box<ReworkAuthorization>>,
        identities: NewWorkRunIdentities,
        product_session_id: ProductSessionId,
        profile: String,
        goal: String,
        attempt: u64,
        now_millis: u64,
    ) -> Result<Self, CoordinationError> {
        let rework_aggregate = rework_authorization
            .as_ref()
            .map(|authorization| authorization.dispatch_aggregate(source))
            .transpose()
            .map_err(|error| {
                CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
            })?;
        let aggregate = rework_aggregate
            .as_ref()
            .unwrap_or(&source.snapshot().work_run_aggregate);
        validate_workrun_dispatch(
            source,
            aggregate,
            rework_authorization.as_deref(),
            &identities,
            &profile,
            &goal,
            attempt,
        )?;
        if source
            .snapshot()
            .attention_items
            .iter()
            .any(|item| item.blocking && item.status == AttentionItemStatus::Open)
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::AttentionRequired,
                "resolve blocking Attention before dispatch",
            ));
        }
        if source.revision() == MAX_SAFE_INTEGER || now_millis < source.snapshot().updated_at_millis
        {
            return Err(CoordinationError::new(
                CoordinationErrorCode::RevisionConflict,
                "WorkRun dispatch timestamp or Delivery revision is stale",
            ));
        }
        let mut snapshot = source.clone().into_snapshot();
        snapshot.revision += 1;
        snapshot.updated_at_millis = now_millis;
        if let Some(aggregate) = rework_aggregate {
            snapshot.work_run_aggregate = aggregate;
            crate::domain::rework::invalidate_candidate_authorization_for_writer_start(
                &mut snapshot,
            );
        }
        let mut intent = ExecutionIntent {
            execution_job_id: identities.execution_job_id.clone(),
            product_session_id,
            delivery_id: source.id().clone(),
            work_contract_id: identities.work_contract_id,
            work_contract_revision: identities.work_contract_revision,
            work_item_id: identities.work_item_id,
            work_item_revision: identities.work_item_revision,
            work_run_id: identities.work_run_id,
            role: profile,
            attempt,
            goal,
            rework_authorization,
            validation_seal: Sha256Digest(String::new()),
        };
        intent.validation_seal = seal_execution_intent(&snapshot, &intent)?;
        let effect = WorkRunStartEffect::Dispatch(Box::new(intent));
        let delivery = Delivery::try_from_snapshot(snapshot).map_err(|error| {
            CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
        })?;
        Ok(Self {
            delivery: delivery.clone(),
            effect: effect.clone(),
            source_delivery: source.clone(),
            sealed_delivery: delivery,
            sealed_effect: effect,
            kind: WorkRunStartKind::WorkRunDispatch,
        })
    }

    /// Returns whether this transition is the stage-free canonical `WorkRun`
    /// dispatch seal.
    #[must_use]
    pub const fn is_canonical_workrun_dispatch(&self) -> bool {
        matches!(self.kind, WorkRunStartKind::WorkRunDispatch)
    }

    /// Checks that the public projection still matches the application-owned
    /// transition. Callers may inspect the projection, but cannot turn an
    /// edited copy into stage-start authority.
    ///
    /// # Errors
    ///
    /// Returns a conflict when either the Delivery or execution effect was
    /// changed after [`Self::canonical_workrun_dispatch`] created this result.
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
        if !matches!(self.kind, WorkRunStartKind::WorkRunDispatch) {
            return Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "only a canonical WorkRun dispatch can be committed",
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
        if self.kind != WorkRunStartKind::Clarify
            || !matches!(self.effect, WorkRunStartEffect::Clarify(_))
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

fn validate_workrun_dispatch(
    source: &Delivery,
    aggregate: &WorkRunAggregate,
    rework_authorization: Option<&ReworkAuthorization>,
    identities: &NewWorkRunIdentities,
    profile: &str,
    goal: &str,
    attempt: u64,
) -> Result<(), CoordinationError> {
    let selected = match rework_authorization {
        Some(authorization) => aggregate.start_item(authorization.work_item_id()),
        None if matches!(profile, "reviewer" | "verifier" | "adversarial-verifier") => {
            aggregate.start_verification(&identities.work_item_id)
        }
        None => aggregate.start_next(),
    }
    .map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::WrongState, format!("{error:?}"))
    })?;
    if identities.work_contract_id != aggregate.contract.id
        || identities.work_contract_revision != aggregate.contract.revision
        || identities.work_item_id != selected.work_item.id
        || identities.work_item_revision != selected.work_item.revision
        || i64::try_from(attempt).ok() != Some(selected.attempt)
        || goal != selected.work_item.goal
        || !matches!(
            profile,
            "executor" | "reviewer" | "verifier" | "adversarial-verifier" | "remediator"
        )
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "WorkRun dispatch must match the selected WorkItem, contract, attempt and profile",
        ));
    }
    if (profile == "remediator") != rework_authorization.is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::WrongState,
            "remediator dispatch requires exactly one sealed rework authorization",
        ));
    }
    if let Some(authorization) = rework_authorization {
        authorization
            .validate_for_dispatch(source)
            .map_err(|error| {
                CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
            })?;
        if i64::try_from(attempt).ok() != Some(authorization.work_run_attempt()) {
            return Err(CoordinationError::new(
                CoordinationErrorCode::Conflict,
                "rework must dispatch the exact failed candidate WorkItem and revision",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelIntent {
    pub work_run_id: WorkRunId,
    pub execution_job_id: ExecutionJobId,
    pub attempt: u64,
    pub product_session_id: ProductSessionId,
    pub worker_session_id: WorkerSessionId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelAcknowledgement {
    pub work_run_id: WorkRunId,
    pub execution_job_id: ExecutionJobId,
    pub attempt: u64,
    pub worker_session_id: WorkerSessionId,
}

/// Consumes the sealed precise-rework decision instead of letting a caller
/// choose a task or silently ignore a clarification result.
///
/// Start decisions are rejected here: they require
/// [`WorkRunStartResult::canonical_workrun_dispatch`]. A bounded/repeated failure
/// changes the Delivery to `Clarifying` without
/// creating a remediator `WorkRun`, `SessionBinding`, or `ExecutionJob`.
///
/// # Errors
///
/// Rejects a stale revision, non-reworking Delivery, active run, unresolved
/// Attention, or an input that tries to combine another authorization or
/// terminal/review facts with the sealed decision.
pub fn advance_rework(
    delivery: &Delivery,
    input: &ReworkAdvanceInput,
    decision: ReworkDecision,
) -> Result<WorkRunStartResult, CoordinationError> {
    if input.rework_authorization.is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::InvalidRequest,
            "advance_rework owns the sealed authorization input",
        ));
    }
    match decision {
        ReworkDecision::Start(authorization) => {
            authorization
                .validate_for_dispatch(delivery)
                .map_err(|error| {
                    CoordinationError::new(
                        CoordinationErrorCode::Conflict,
                        format!("rework dispatch authorization is stale: {error}"),
                    )
                })?;
            Err(CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "rework dispatch requires the canonical WorkRun dispatch path",
            ))
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
                || delivery
                    .snapshot()
                    .work_run_aggregate
                    .runs
                    .iter()
                    .any(is_active)
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
            let effect = WorkRunStartEffect::Clarify(reason);
            Ok(WorkRunStartResult {
                source_delivery: delivery.clone(),
                sealed_delivery: clarified_delivery.clone(),
                sealed_effect: effect.clone(),
                delivery: clarified_delivery,
                effect,
                kind: WorkRunStartKind::Clarify,
            })
        }
    }
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
        work_contract_id: &intent.work_contract_id,
        work_contract_revision: &intent.work_contract_revision,
        work_item_id: &intent.work_item_id,
        work_item_revision: &intent.work_item_revision,
        work_run_id: &intent.work_run_id,
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

/// Creates cancellation intent for one explicitly selected active `WorkRun`.
/// The Control Plane must persist this effect before sending it to the Worker.
///
/// # Errors
/// Rejects stale revisions, inactive runs, or incomplete accepted bindings.
pub fn request_cancel(
    delivery: &Delivery,
    expected_revision: u64,
    work_run_id: &WorkRunId,
) -> Result<CancelIntent, CoordinationError> {
    if delivery.revision() != expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before cancellation",
        ));
    }
    let run = delivery
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| {
            &run.id == work_run_id
                && matches!(run.state, WorkRunState::Leased | WorkRunState::Running)
        })
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::WrongState,
                "cancellation requires the selected active WorkRun",
            )
        })?;
    let mut bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| &binding.work_run_id == work_run_id);
    let binding = bindings.next().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "cancellation WorkRun has no binding",
        )
    })?;
    if bindings.next().is_some()
        || binding.delivery_id != *delivery.id()
        || binding.work_contract_id != run.work_contract_id
        || binding.work_contract_revision != run.contract_revision
        || binding.work_item_id != run.work_item_id
        || binding.work_item_revision != run.work_item_revision
        || binding.execution_job_id != run.execution_job_id
        || i64::try_from(binding.attempt).ok() != Some(run.attempt)
        || Some(&binding.product_session_id) != run.product_session_id.as_ref()
        || binding.worker_session_id.as_ref() != Some(&run.worker_session_id)
        || binding.worker_id.as_ref() != Some(&run.worker_id)
        || binding.worker_instance_id.as_ref() != Some(&run.worker_instance_id)
        || binding.lease_id.as_ref() != Some(&run.lease_id)
        || binding.fencing_token.as_ref().map(|token| token.0.as_str())
            != Some(run.fencing_token.as_str())
        || binding.codex_thread_id != run.codex_thread_id
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "cancellation requires the exact accepted WorkRun binding",
        ));
    }
    Ok(CancelIntent {
        work_run_id: run.id.clone(),
        execution_job_id: run.execution_job_id.clone(),
        attempt: binding.attempt,
        product_session_id: binding.product_session_id.clone(),
        worker_session_id: run.worker_session_id.clone(),
    })
}

/// Validates receipt of cancellation without settling the `WorkRun`.
/// Only a verified terminal outcome may finish execution.
///
/// # Errors
/// Rejects another run, a replaced attempt, or a changed current binding.
pub fn acknowledge_cancel(
    delivery: &Delivery,
    intent: &CancelIntent,
    acknowledgement: &CancelAcknowledgement,
) -> Result<Delivery, CoordinationError> {
    if acknowledgement.work_run_id != intent.work_run_id
        || acknowledgement.execution_job_id != intent.execution_job_id
        || acknowledgement.attempt != intent.attempt
        || acknowledgement.worker_session_id != intent.worker_session_id
        || request_cancel(delivery, delivery.revision(), &intent.work_run_id)? != *intent
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "cancellation acknowledgement does not match the current requested WorkRun",
        ));
    }
    Ok(delivery.clone())
}

fn revalidate_terminal_outcome(
    delivery: &Delivery,
    active_lease: &ActiveLeaseIdentity,
    outcome: &VerifiedTerminalOutcome,
) -> Result<VerifiedTerminalOutcome, CoordinationError> {
    if &outcome.lease_identity != active_lease {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "terminal outcome was verified for another active lease",
        ));
    }
    // Recheck the complete persisted WorkRun and binding, not just the lease
    // copied into a previously verified result.
    verify_work_run_terminal(
        delivery,
        active_lease,
        TerminalWorkerOutcome {
            work_run_id: outcome.work_run_id.clone(),
            execution_job_id: outcome.execution_job_id().clone(),
            attempt: outcome.attempt(),
            lease_id: outcome.lease_id().clone(),
            fencing_token: outcome.fencing_token().clone(),
            worker_id: outcome.worker_id().clone(),
            worker_instance_id: outcome.worker_instance_id().clone(),
            worker_session_id: outcome.worker_session_id().clone(),
            status: outcome.status,
            metadata: outcome.metadata.clone(),
        },
        false,
    )
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
    let verified = revalidate_terminal_outcome(delivery, active_lease, outcome)?;
    let mut snapshot = delivery.clone().into_snapshot();
    let read_only = snapshot.session_bindings.iter().any(|binding| {
        binding.work_run_id == *verified.work_run_id()
            && binding.execution_job_id == *verified.execution_job_id()
            && matches!(
                binding.execution_profile.as_deref(),
                Some("reviewer" | "verifier" | "adversarial-verifier")
            )
    });
    let settlement = if read_only {
        snapshot
            .work_run_aggregate
            .settle_verification_outcome(&verified)
    } else {
        snapshot
            .work_run_aggregate
            .settle_verified_outcome(&verified)
    };
    settlement.map_err(|error| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            format!("terminal WorkRun settlement rejected: {error:?}"),
        )
    })?;
    snapshot.revision = snapshot
        .revision
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::RevisionConflict,
                "Delivery revision is exhausted",
            )
        })?;
    snapshot.updated_at_millis = outcome.finished_at_millis();
    Delivery::try_from_snapshot(snapshot)
        .map_err(|error| CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string()))
}

fn is_active(run: &WorkRun) -> bool {
    matches!(
        run.state,
        WorkRunState::Queued | WorkRunState::Leased | WorkRunState::Running
    )
}

#[cfg(test)]
mod canonical_terminal_authority_tests {
    use super::*;

    #[test]
    fn terminal_authority_uses_explicit_workrun_and_rejects_changed_lease() {
        let mut snapshot = crate::domain::test_fixture();
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.attention_items.clear();
        snapshot.status = DeliveryStatus::Draft;
        snapshot.work_run_aggregate.runs[0].state = WorkRunState::Running;
        snapshot.work_run_aggregate.items[0].state = winwincode_domain::WorkItemState::InProgress;
        snapshot.session_bindings[0].execution_profile = Some("executor".into());
        snapshot.session_bindings[0]
            .runtime_context
            .as_mut()
            .expect("fixture runtime context")
            .agent_identity
            .role = "executor".into();
        let delivery = Delivery::try_from_snapshot(snapshot).expect("valid canonical fixture");
        let run = &delivery.snapshot().work_run_aggregate.runs[0];
        let binding = &delivery.snapshot().session_bindings[0];
        let lease = ActiveLeaseIdentity {
            execution_job_id: run.execution_job_id.clone(),
            attempt: u64::try_from(run.attempt).expect("positive attempt"),
            lease_id: run.lease_id.clone(),
            fencing_token: FencingToken(run.fencing_token.clone()),
            worker_id: run.worker_id.clone(),
            worker_instance_id: run.worker_instance_id.clone(),
            worker_session_id: run.worker_session_id.clone(),
        };
        let outcome = TerminalWorkerOutcome {
            work_run_id: run.id.clone(),
            execution_job_id: lease.execution_job_id.clone(),
            attempt: lease.attempt,
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            worker_session_id: lease.worker_session_id.clone(),
            status: TerminalOutcomeStatus::Succeeded,
            metadata: TerminalOutcomeMetadata {
                codex_thread_id: binding.codex_thread_id.clone(),
                finished_at_millis: delivery
                    .snapshot()
                    .updated_at_millis
                    .max(binding.bound_at_millis)
                    + 1,
                last_event_sequence: ExecutionAckSequence(1),
                artifacts: Vec::new(),
            },
        };
        let verified = verify_terminal_outcome(&delivery, &lease, outcome.clone())
            .expect("exact WorkRun terminal authority");
        assert_eq!(verified.work_run_id(), &run.id);
        let mut changed_binding = delivery.clone().into_snapshot();
        changed_binding.session_bindings[0].lease_id =
            Some(LeaseId("lse_01J00000000000000000000999".into()));
        let changed_binding = Delivery::try_from_snapshot(changed_binding)
            .expect("valid shape, mismatched authority");
        let before = changed_binding.clone();
        assert!(
            apply_terminal_outcome(
                &changed_binding,
                changed_binding.revision(),
                &lease,
                &verified
            )
            .is_err()
        );
        assert_eq!(changed_binding, before);
        let settled = apply_terminal_outcome(&delivery, delivery.revision(), &lease, &verified)
            .expect("settle the exact WorkRun");
        assert_eq!(
            settled.snapshot().work_run_aggregate.runs[0].state,
            WorkRunState::CandidateReady
        );
        assert_eq!(
            settled.snapshot().work_run_aggregate.items[0].state,
            winwincode_domain::WorkItemState::CandidateReady
        );
        assert!(apply_terminal_outcome(&settled, settled.revision(), &lease, &verified).is_err());

        let mut wrong_run = outcome.clone();
        wrong_run.work_run_id = WorkRunId("wrn_01J00000000000000000000999".into());
        assert!(verify_terminal_outcome(&delivery, &lease, wrong_run).is_err());
        let mut stale_lease = lease;
        stale_lease.fencing_token = FencingToken("999".into());
        assert!(verify_terminal_outcome(&delivery, &stale_lease, outcome).is_err());
    }
}
