// SPDX-License-Identifier: Apache-2.0

//! Deterministic candidate identity derived from sealed Control Plane facts.
//!
//! This module does not read Git, Worker messages, or API payloads. The future
//! Git/Artifact adapter owns construction of [`ValidatedGitSnapshotFact`].
//! Until that adapter exists, only crate-private unit-test support can create a
//! sealed snapshot, so production candidate freezing remains fail closed.

use std::collections::HashSet;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::application::stage::{TerminalOutcomeStatus, VerifiedTerminalOutcome};

use super::{
    Delivery, DeliveryStage, DeliveryValidationError, DeliveryValidationErrorCode, RepositoryRef,
    SessionBinding, SessionBindingId, StageRun, StageRunActorType, StageRunStatus, bounded_text,
    validation_error,
};
use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, FencingToken, LeaseId,
    ProductSessionId, Sha256Digest, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

const MAX_CHANGED_PATHS: usize = 100_000;
const MAX_PATH_LENGTH: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidatePathState {
    Present,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidatePathFact {
    pub path: String,
    pub state: CandidatePathState,
    pub object_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateHunkFact {
    pub file_path: String,
    /// Digest of the resulting diff hunk content.
    pub hunk_sha256: String,
    /// Original reviewed hunk identity whose range this result modifies.
    ///
    /// Present only for a remediator old-to-new delta. The trusted Git adapter
    /// derives this mapping from hunk ranges; it is not copied from a prompt.
    pub source_hunk_sha256: Option<String>,
}

/// A Git/Artifact-adapter fact for one exact Job workspace snapshot.
///
/// All fields and constructors are private. This type is intentionally not
/// deserializable: callers and Workers cannot promote format-valid strings into
/// candidate facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedGitSnapshotFact {
    stage_run_id: StageRunId,
    session_binding_id: SessionBindingId,
    product_session_id: ProductSessionId,
    execution_job_id: ExecutionJobId,
    attempt: u64,
    lease_id: LeaseId,
    fencing_token: FencingToken,
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    worker_session_id: WorkerSessionId,
    codex_thread_id: CodexThreadId,
    repository: RepositoryRef,
    base_commit_id: String,
    base_tree_id: String,
    candidate_commit_id: String,
    candidate_tree_id: String,
    diff_sha256: String,
    changed_paths: Vec<CandidatePathFact>,
    changed_hunks: Vec<CandidateHunkFact>,
    artifact_ref: String,
    artifact_digest: Sha256Digest,
    last_event_sequence: u64,
    finished_at_millis: u64,
    validation_seal: [u8; 32],
}

impl ValidatedGitSnapshotFact {
    pub(crate) fn stage_run_id(&self) -> &StageRunId {
        &self.stage_run_id
    }

    pub(crate) fn session_binding_id(&self) -> &SessionBindingId {
        &self.session_binding_id
    }

    pub(crate) fn product_session_id(&self) -> &ProductSessionId {
        &self.product_session_id
    }

    pub(crate) fn execution_job_id(&self) -> &ExecutionJobId {
        &self.execution_job_id
    }

    pub(crate) const fn attempt(&self) -> u64 {
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

    pub(crate) fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub(crate) fn base_commit_id(&self) -> &str {
        &self.base_commit_id
    }

    pub(crate) fn base_tree_id(&self) -> &str {
        &self.base_tree_id
    }

    pub(crate) fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    pub(crate) fn candidate_tree_id(&self) -> &str {
        &self.candidate_tree_id
    }

    pub(crate) fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

    pub(crate) fn changed_paths(&self) -> &[CandidatePathFact] {
        &self.changed_paths
    }

    pub(crate) fn changed_hunks(&self) -> &[CandidateHunkFact] {
        &self.changed_hunks
    }

    pub(crate) fn artifact_ref(&self) -> &str {
        &self.artifact_ref
    }

    pub(crate) fn artifact_digest(&self) -> &Sha256Digest {
        &self.artifact_digest
    }

    pub(crate) const fn last_event_sequence(&self) -> u64 {
        self.last_event_sequence
    }

    pub(crate) const fn finished_at_millis(&self) -> u64 {
        self.finished_at_millis
    }

    pub(crate) fn has_same_terminal_workspace(&self, other: &Self) -> bool {
        self.stage_run_id == other.stage_run_id
            && self.session_binding_id == other.session_binding_id
            && self.product_session_id == other.product_session_id
            && self.execution_job_id == other.execution_job_id
            && self.attempt == other.attempt
            && self.lease_id == other.lease_id
            && self.fencing_token == other.fencing_token
            && self.worker_id == other.worker_id
            && self.worker_instance_id == other.worker_instance_id
            && self.worker_session_id == other.worker_session_id
            && self.codex_thread_id == other.codex_thread_id
            && self.repository == other.repository
            && self.candidate_commit_id == other.candidate_commit_id
            && self.candidate_tree_id == other.candidate_tree_id
            && self.artifact_ref == other.artifact_ref
            && self.artifact_digest == other.artifact_digest
            && self.last_event_sequence == other.last_event_sequence
            && self.finished_at_millis == other.finished_at_millis
    }
}

/// Sealed input accepted by [`freeze_delivery_candidate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreezeCandidateFacts {
    git_snapshot: ValidatedGitSnapshotFact,
    terminal_outcome: VerifiedTerminalOutcome,
}

impl FreezeCandidateFacts {
    pub(crate) fn git_snapshot(&self) -> &ValidatedGitSnapshotFact {
        &self.git_snapshot
    }
}

/// One immutable candidate identity derived from canonical Delivery and sealed
/// Git/Worker facts. It is not an eleventh persisted Delivery object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrozenDeliveryCandidate {
    candidate_ref: String,
    delivery_id: DeliveryId,
    delivery_spec_id: super::DeliverySpecId,
    delivery_spec_revision: u64,
    repository: RepositoryRef,
    base_revision: String,
    producer_delivery_task_id: Option<DeliveryTaskId>,
    producer_stage_run_id: StageRunId,
    producer_stage: DeliveryStage,
    producer_role: String,
    producer_attempt: u64,
    producer_session_binding_id: SessionBindingId,
    producer_product_session_id: ProductSessionId,
    producer_execution_job_id: ExecutionJobId,
    producer_worker_session_id: WorkerSessionId,
    producer_codex_thread_id: CodexThreadId,
    producer_lease_id: LeaseId,
    producer_fencing_token: FencingToken,
    producer_worker_id: WorkerId,
    producer_worker_instance_id: WorkerInstanceId,
    producer_artifact_ref: String,
    producer_artifact_digest: Sha256Digest,
    producer_last_event_sequence: u64,
    producer_finished_at_millis: u64,
    base_commit_id: String,
    base_tree_id: String,
    candidate_commit_id: String,
    candidate_tree_id: String,
    diff_sha256: String,
    changed_paths: Vec<CandidatePathFact>,
}

impl FrozenDeliveryCandidate {
    pub fn candidate_ref(&self) -> &str {
        &self.candidate_ref
    }

    pub fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    pub fn delivery_spec_id(&self) -> &super::DeliverySpecId {
        &self.delivery_spec_id
    }

    pub const fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }

    pub fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub fn producer_delivery_task_id(&self) -> Option<&DeliveryTaskId> {
        self.producer_delivery_task_id.as_ref()
    }

    pub fn producer_stage_run_id(&self) -> &StageRunId {
        &self.producer_stage_run_id
    }

    pub const fn producer_stage(&self) -> DeliveryStage {
        self.producer_stage
    }

    pub fn producer_role(&self) -> &str {
        &self.producer_role
    }

    pub const fn producer_attempt(&self) -> u64 {
        self.producer_attempt
    }

    pub fn producer_session_binding_id(&self) -> &SessionBindingId {
        &self.producer_session_binding_id
    }

    pub fn producer_product_session_id(&self) -> &ProductSessionId {
        &self.producer_product_session_id
    }

    pub fn producer_execution_job_id(&self) -> &ExecutionJobId {
        &self.producer_execution_job_id
    }

    pub fn producer_worker_session_id(&self) -> &WorkerSessionId {
        &self.producer_worker_session_id
    }

    pub fn producer_codex_thread_id(&self) -> &CodexThreadId {
        &self.producer_codex_thread_id
    }

    pub fn producer_lease_id(&self) -> &LeaseId {
        &self.producer_lease_id
    }

    pub fn producer_fencing_token(&self) -> &FencingToken {
        &self.producer_fencing_token
    }

    pub fn producer_worker_id(&self) -> &WorkerId {
        &self.producer_worker_id
    }

    pub fn producer_worker_instance_id(&self) -> &WorkerInstanceId {
        &self.producer_worker_instance_id
    }

    pub fn producer_artifact_ref(&self) -> &str {
        &self.producer_artifact_ref
    }

    pub fn producer_artifact_digest(&self) -> &Sha256Digest {
        &self.producer_artifact_digest
    }

    pub const fn producer_last_event_sequence(&self) -> u64 {
        self.producer_last_event_sequence
    }

    pub const fn producer_finished_at_millis(&self) -> u64 {
        self.producer_finished_at_millis
    }

    pub fn base_commit_id(&self) -> &str {
        &self.base_commit_id
    }

    pub fn base_tree_id(&self) -> &str {
        &self.base_tree_id
    }

    pub fn candidate_commit_id(&self) -> &str {
        &self.candidate_commit_id
    }

    pub fn candidate_tree_id(&self) -> &str {
        &self.candidate_tree_id
    }

    pub fn diff_sha256(&self) -> &str {
        &self.diff_sha256
    }

    pub fn changed_paths(&self) -> &[CandidatePathFact] {
        &self.changed_paths
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GitSnapshotSealIdentity<'fact> {
    stage_run_id: &'fact StageRunId,
    session_binding_id: &'fact SessionBindingId,
    product_session_id: &'fact ProductSessionId,
    execution_job_id: &'fact ExecutionJobId,
    attempt: u64,
    lease_id: &'fact LeaseId,
    fencing_token: &'fact FencingToken,
    worker_id: &'fact WorkerId,
    worker_instance_id: &'fact WorkerInstanceId,
    worker_session_id: &'fact WorkerSessionId,
    codex_thread_id: &'fact CodexThreadId,
    repository: &'fact RepositoryRef,
    base_commit_id: &'fact str,
    base_tree_id: &'fact str,
    candidate_commit_id: &'fact str,
    candidate_tree_id: &'fact str,
    diff_sha256: &'fact str,
    changed_paths: &'fact [CandidatePathFact],
    changed_hunks: &'fact [CandidateHunkFact],
    artifact_ref: &'fact str,
    artifact_digest: &'fact Sha256Digest,
    last_event_sequence: u64,
    finished_at_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateIdentity<'candidate> {
    delivery_id: &'candidate DeliveryId,
    delivery_spec_id: &'candidate super::DeliverySpecId,
    delivery_spec_revision: u64,
    repository: &'candidate RepositoryRef,
    base_revision: &'candidate str,
    producer_delivery_task_id: Option<&'candidate DeliveryTaskId>,
    producer_stage_run_id: &'candidate StageRunId,
    producer_stage: DeliveryStage,
    producer_role: &'candidate str,
    producer_attempt: u64,
    producer_session_binding_id: &'candidate SessionBindingId,
    producer_product_session_id: &'candidate ProductSessionId,
    producer_execution_job_id: &'candidate ExecutionJobId,
    producer_worker_session_id: &'candidate WorkerSessionId,
    producer_codex_thread_id: &'candidate CodexThreadId,
    producer_lease_id: &'candidate LeaseId,
    producer_fencing_token: &'candidate FencingToken,
    producer_worker_id: &'candidate WorkerId,
    producer_worker_instance_id: &'candidate WorkerInstanceId,
    producer_artifact_ref: &'candidate str,
    producer_artifact_digest: &'candidate Sha256Digest,
    producer_last_event_sequence: u64,
    producer_finished_at_millis: u64,
    base_commit_id: &'candidate str,
    base_tree_id: &'candidate str,
    candidate_commit_id: &'candidate str,
    candidate_tree_id: &'candidate str,
    diff_sha256: &'candidate str,
    changed_paths: &'candidate [CandidatePathFact],
}

/// Freezes the current successful executor writer from sealed facts.
///
/// Remediator output has a different trust boundary: callers must use the
/// authorization-bound replacement entry in the rework module. Keeping that
/// path separate makes it impossible to promote an unchecked rework result to
/// the current candidate.
///
/// # Errors
///
/// Rejects stale canonical state, a non-successful or cross-Job terminal
/// outcome, a modified seal, malformed Git facts, or a snapshot from another
/// repository, session, attempt, lease, fence, or Worker.
pub fn freeze_delivery_candidate(
    delivery: &Delivery,
    facts: &FreezeCandidateFacts,
) -> Result<FrozenDeliveryCandidate, DeliveryValidationError> {
    freeze_candidate_for_stage(delivery, facts, DeliveryStage::Executing)
}

pub(crate) fn freeze_authorized_rework_candidate(
    delivery: &Delivery,
    facts: &FreezeCandidateFacts,
) -> Result<FrozenDeliveryCandidate, DeliveryValidationError> {
    freeze_candidate_for_stage(delivery, facts, DeliveryStage::Reworking)
}

fn freeze_candidate_for_stage(
    delivery: &Delivery,
    facts: &FreezeCandidateFacts,
    required_stage: DeliveryStage,
) -> Result<FrozenDeliveryCandidate, DeliveryValidationError> {
    let snapshot = &facts.git_snapshot;
    let producer = current_writer(delivery, &snapshot.stage_run_id)?;
    if producer.stage != required_stage {
        return Err(stale_candidate(match required_stage {
            DeliveryStage::Executing => {
                "generic candidate freeze accepts only an executor; remediator output requires authorization"
            }
            DeliveryStage::Reworking => {
                "authorized replacement freeze requires one remediator producer"
            }
            _ => "candidate freeze received an unsupported writer stage",
        }));
    }
    let binding = exact_producer_binding(delivery, producer, &snapshot.session_binding_id)?;

    assert_validated_git_snapshot_fact(snapshot)?;
    validate_git_snapshot(delivery, snapshot)?;
    verify_terminal_snapshot_binding(producer, binding, &facts.terminal_outcome, snapshot)?;

    let mut changed_paths = snapshot.changed_paths.clone();
    changed_paths.sort_by(|left, right| left.path.cmp(&right.path));
    let mut candidate = FrozenDeliveryCandidate {
        candidate_ref: String::new(),
        delivery_id: delivery.id().clone(),
        delivery_spec_id: delivery.snapshot().spec.id.clone(),
        delivery_spec_revision: delivery.snapshot().spec.revision,
        repository: delivery.snapshot().spec.repository.clone(),
        base_revision: delivery.snapshot().spec.base_revision.clone(),
        producer_delivery_task_id: producer.delivery_task_id.clone(),
        producer_stage_run_id: producer.id.clone(),
        producer_stage: producer.stage,
        producer_role: producer.role.clone(),
        producer_attempt: producer.attempt,
        producer_session_binding_id: binding.id.clone(),
        producer_product_session_id: binding.product_session_id.clone(),
        producer_execution_job_id: binding.execution_job_id.clone(),
        producer_worker_session_id: binding
            .worker_session_id
            .clone()
            .ok_or_else(|| stale_candidate("candidate producer WorkerSession is missing"))?,
        producer_codex_thread_id: binding
            .codex_thread_id
            .clone()
            .ok_or_else(|| stale_candidate("candidate producer CodexThread is missing"))?,
        producer_lease_id: snapshot.lease_id.clone(),
        producer_fencing_token: snapshot.fencing_token.clone(),
        producer_worker_id: snapshot.worker_id.clone(),
        producer_worker_instance_id: snapshot.worker_instance_id.clone(),
        producer_artifact_ref: snapshot.artifact_ref.clone(),
        producer_artifact_digest: snapshot.artifact_digest.clone(),
        producer_last_event_sequence: snapshot.last_event_sequence,
        producer_finished_at_millis: snapshot.finished_at_millis,
        base_commit_id: snapshot.base_commit_id.clone(),
        base_tree_id: snapshot.base_tree_id.clone(),
        candidate_commit_id: snapshot.candidate_commit_id.clone(),
        candidate_tree_id: snapshot.candidate_tree_id.clone(),
        diff_sha256: snapshot.diff_sha256.clone(),
        changed_paths,
    };
    candidate.candidate_ref = candidate_reference(&candidate)?;
    Ok(candidate)
}

/// Rejects a frozen candidate after a spec/fact change or later writer starts.
///
/// # Errors
///
/// Returns an error when the candidate is no longer the exact current derived
/// candidate for the Delivery.
pub(crate) fn assert_frozen_candidate_current(
    delivery: &Delivery,
    candidate: &FrozenDeliveryCandidate,
) -> Result<(), DeliveryValidationError> {
    if candidate.delivery_id != *delivery.id()
        || candidate.delivery_spec_id != delivery.snapshot().spec.id
        || candidate.delivery_spec_revision != delivery.snapshot().spec.revision
        || candidate.repository != delivery.snapshot().spec.repository
        || candidate.base_revision != delivery.snapshot().spec.base_revision
    {
        return Err(stale_candidate(
            "candidate does not match the current DeliverySpec",
        ));
    }
    let producer = current_writer(delivery, &candidate.producer_stage_run_id)
        .map_err(|_| stale_candidate("candidate producer is no longer current"))?;
    let binding =
        exact_producer_binding(delivery, producer, &candidate.producer_session_binding_id)
            .map_err(|_| stale_candidate("candidate producer binding is no longer current"))?;
    let same_writer = candidate.producer_delivery_task_id == producer.delivery_task_id
        && candidate.producer_stage == producer.stage
        && candidate.producer_role == producer.role
        && candidate.producer_attempt == producer.attempt
        && candidate.producer_product_session_id == binding.product_session_id
        && candidate.producer_execution_job_id == binding.execution_job_id
        && binding.worker_session_id.as_ref() == Some(&candidate.producer_worker_session_id)
        && binding.codex_thread_id.as_ref() == Some(&candidate.producer_codex_thread_id)
        && producer.finished_at_millis == Some(candidate.producer_finished_at_millis);
    if !same_writer {
        return Err(stale_candidate(
            "candidate writer or complete SessionBinding identity changed",
        ));
    }
    if candidate.candidate_ref != candidate_reference(candidate)? {
        return Err(stale_candidate("candidate facts changed after freezing"));
    }
    Ok(())
}

fn candidate_reference(
    candidate: &FrozenDeliveryCandidate,
) -> Result<String, DeliveryValidationError> {
    let encoded = serde_json::to_vec(&CandidateIdentity::from(candidate)).map_err(|error| {
        validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            "candidate",
            format!("candidate identity cannot be encoded: {error}"),
        )
    })?;
    Ok(format!(
        "git-candidate:sha256:{:x}",
        Sha256::digest(encoded)
    ))
}

impl<'candidate> From<&'candidate FrozenDeliveryCandidate> for CandidateIdentity<'candidate> {
    fn from(candidate: &'candidate FrozenDeliveryCandidate) -> Self {
        Self {
            delivery_id: &candidate.delivery_id,
            delivery_spec_id: &candidate.delivery_spec_id,
            delivery_spec_revision: candidate.delivery_spec_revision,
            repository: &candidate.repository,
            base_revision: &candidate.base_revision,
            producer_delivery_task_id: candidate.producer_delivery_task_id.as_ref(),
            producer_stage_run_id: &candidate.producer_stage_run_id,
            producer_stage: candidate.producer_stage,
            producer_role: &candidate.producer_role,
            producer_attempt: candidate.producer_attempt,
            producer_session_binding_id: &candidate.producer_session_binding_id,
            producer_product_session_id: &candidate.producer_product_session_id,
            producer_execution_job_id: &candidate.producer_execution_job_id,
            producer_worker_session_id: &candidate.producer_worker_session_id,
            producer_codex_thread_id: &candidate.producer_codex_thread_id,
            producer_lease_id: &candidate.producer_lease_id,
            producer_fencing_token: &candidate.producer_fencing_token,
            producer_worker_id: &candidate.producer_worker_id,
            producer_worker_instance_id: &candidate.producer_worker_instance_id,
            producer_artifact_ref: &candidate.producer_artifact_ref,
            producer_artifact_digest: &candidate.producer_artifact_digest,
            producer_last_event_sequence: candidate.producer_last_event_sequence,
            producer_finished_at_millis: candidate.producer_finished_at_millis,
            base_commit_id: &candidate.base_commit_id,
            base_tree_id: &candidate.base_tree_id,
            candidate_commit_id: &candidate.candidate_commit_id,
            candidate_tree_id: &candidate.candidate_tree_id,
            diff_sha256: &candidate.diff_sha256,
            changed_paths: &candidate.changed_paths,
        }
    }
}

fn current_writer<'delivery>(
    delivery: &'delivery Delivery,
    producer_id: &StageRunId,
) -> Result<&'delivery StageRun, DeliveryValidationError> {
    let runs = &delivery.snapshot().stage_runs;
    let (producer_index, producer) = runs
        .iter()
        .enumerate()
        .find(|(_, run)| &run.id == producer_id)
        .ok_or_else(|| stale_candidate("candidate producer StageRun is missing"))?;
    let valid_role = matches!(
        (producer.stage, producer.role.as_str()),
        (DeliveryStage::Executing, "executor") | (DeliveryStage::Reworking, "remediator")
    );
    if producer.actor_type != StageRunActorType::Codex
        || producer.status != StageRunStatus::Succeeded
        || producer.delivery_task_id.is_none()
        || !valid_role
    {
        return Err(stale_candidate(
            "candidate producer must be one task-scoped successful Codex executor or remediator",
        ));
    }
    let later_writer = runs.iter().enumerate().any(|(index, run)| {
        run.id != producer.id
            && matches!(
                run.stage,
                DeliveryStage::Executing | DeliveryStage::Reworking
            )
            && (index > producer_index
                || run.started_at_millis > producer.started_at_millis
                || (run.started_at_millis == producer.started_at_millis
                    && run.attempt >= producer.attempt))
    });
    if later_writer {
        return Err(stale_candidate(
            "a later executor or remediator writer already started",
        ));
    }
    Ok(producer)
}

fn exact_producer_binding<'delivery>(
    delivery: &'delivery Delivery,
    producer: &StageRun,
    binding_id: &SessionBindingId,
) -> Result<&'delivery SessionBinding, DeliveryValidationError> {
    let bindings = delivery
        .snapshot()
        .session_bindings
        .iter()
        .filter(|binding| binding.stage_run_id == producer.id)
        .collect::<Vec<_>>();
    if bindings.len() != 1 || &bindings[0].id != binding_id {
        return Err(stale_candidate(
            "candidate producer must have exactly one referenced SessionBinding",
        ));
    }
    let binding = bindings[0];
    let complete = binding.worker_session_id.is_some() && binding.codex_thread_id.is_some();
    if binding.delivery_id != *delivery.id()
        || binding.delivery_task_id != producer.delivery_task_id
        || !complete
        || binding.bound_at_millis < producer.started_at_millis
        || producer
            .finished_at_millis
            .is_none_or(|finished| binding.bound_at_millis > finished)
    {
        return Err(stale_candidate(
            "candidate producer does not have one exact complete SessionBinding",
        ));
    }
    Ok(binding)
}

pub(crate) fn assert_validated_git_snapshot_fact(
    snapshot: &ValidatedGitSnapshotFact,
) -> Result<(), DeliveryValidationError> {
    let expected = seal_git_snapshot(snapshot)?;
    if snapshot.validation_seal == expected {
        validate_git_snapshot_shape(snapshot)
    } else {
        Err(invalid_candidate(
            "candidate requires an unchanged sealed ValidatedGitSnapshotFact",
        ))
    }
}

fn validate_git_snapshot(
    delivery: &Delivery,
    snapshot: &ValidatedGitSnapshotFact,
) -> Result<(), DeliveryValidationError> {
    if snapshot.repository != delivery.snapshot().spec.repository {
        return Err(invalid_candidate(
            "candidate Git snapshot belongs to another repository",
        ));
    }
    if git_object_id(&delivery.snapshot().spec.base_revision)
        && delivery.snapshot().spec.base_revision != snapshot.base_commit_id
    {
        return Err(invalid_candidate(
            "candidate base commit does not match DeliverySpec.baseRevision",
        ));
    }
    Ok(())
}

fn validate_git_snapshot_shape(
    snapshot: &ValidatedGitSnapshotFact,
) -> Result<(), DeliveryValidationError> {
    let object_ids = [
        snapshot.base_commit_id.as_str(),
        snapshot.base_tree_id.as_str(),
        snapshot.candidate_commit_id.as_str(),
        snapshot.candidate_tree_id.as_str(),
    ];
    if object_ids.iter().any(|value| !git_object_id(value))
        || !lowercase_sha256(&snapshot.diff_sha256)
        || !snapshot
            .artifact_digest
            .0
            .strip_prefix("sha256:")
            .is_some_and(lowercase_sha256)
    {
        return Err(invalid_candidate(
            "sealed Git objects, diff, and artifact must use lowercase hexadecimal identities",
        ));
    }
    let object_length = object_ids[0].len();
    if object_ids.iter().any(|value| value.len() != object_length) {
        return Err(invalid_candidate(
            "candidate Git object identities must use one repository object format",
        ));
    }
    bounded_text(&snapshot.artifact_ref, "candidate.artifactRef", 4_096)?;
    if snapshot.last_event_sequence == 0 {
        return Err(invalid_candidate(
            "candidate Job snapshot requires a terminal event sequence",
        ));
    }
    if snapshot.changed_paths.len() > MAX_CHANGED_PATHS {
        return Err(invalid_candidate(
            "candidate changed paths exceed the supported limit",
        ));
    }
    let mut paths = HashSet::with_capacity(snapshot.changed_paths.len());
    for fact in &snapshot.changed_paths {
        if !portable_path(&fact.path) || !paths.insert(fact.path.as_str()) {
            return Err(invalid_candidate(
                "candidate changed paths must be unique portable repository-relative paths",
            ));
        }
        let valid_object = fact
            .object_id
            .as_deref()
            .is_some_and(|object_id| git_object_id(object_id) && object_id.len() == object_length);
        if (fact.state == CandidatePathState::Present) != valid_object
            || (fact.state == CandidatePathState::Deleted && fact.object_id.is_some())
        {
            return Err(invalid_candidate(
                "candidate path state does not match its Git object identity",
            ));
        }
    }
    let mut hunks = HashSet::with_capacity(snapshot.changed_hunks.len());
    for hunk in &snapshot.changed_hunks {
        if !portable_path(&hunk.file_path)
            || !lowercase_sha256(&hunk.hunk_sha256)
            || hunk
                .source_hunk_sha256
                .as_deref()
                .is_some_and(|source| !lowercase_sha256(source))
            || !paths.contains(hunk.file_path.as_str())
            || !hunks.insert((hunk.file_path.as_str(), hunk.hunk_sha256.as_str()))
        {
            return Err(invalid_candidate(
                "sealed Git hunks must be unique and belong to one changed path",
            ));
        }
    }
    Ok(())
}

fn verify_terminal_snapshot_binding(
    producer: &StageRun,
    binding: &SessionBinding,
    outcome: &VerifiedTerminalOutcome,
    snapshot: &ValidatedGitSnapshotFact,
) -> Result<(), DeliveryValidationError> {
    let last_event_sequence = i64::try_from(snapshot.last_event_sequence).ok();
    let exact_artifact = outcome.artifacts().iter().any(|artifact| {
        artifact.artifact_id.0 == snapshot.artifact_ref
            && artifact.digest == snapshot.artifact_digest
    });
    let exact = outcome.status() == TerminalOutcomeStatus::Succeeded
        && outcome.stage_run_id() == &producer.id
        && outcome.execution_job_id() == &binding.execution_job_id
        && outcome.attempt() == producer.attempt
        && outcome.worker_session_id() == snapshot.worker_session_id()
        && outcome.lease_id() == snapshot.lease_id()
        && outcome.fencing_token() == snapshot.fencing_token()
        && outcome.worker_id() == snapshot.worker_id()
        && outcome.worker_instance_id() == snapshot.worker_instance_id()
        && outcome.codex_thread_id() == Some(snapshot.codex_thread_id())
        && Some(outcome.last_event_sequence().0) == last_event_sequence
        && outcome.finished_at_millis() == snapshot.finished_at_millis
        && exact_artifact
        && snapshot.stage_run_id == producer.id
        && snapshot.session_binding_id == binding.id
        && snapshot.product_session_id == binding.product_session_id
        && snapshot.execution_job_id == binding.execution_job_id
        && snapshot.attempt == producer.attempt
        && binding.worker_session_id.as_ref() == Some(&snapshot.worker_session_id)
        && binding.codex_thread_id.as_ref() == Some(&snapshot.codex_thread_id)
        && producer.finished_at_millis == Some(snapshot.finished_at_millis);
    if exact {
        Ok(())
    } else {
        Err(stale_candidate(
            "sealed candidate snapshot does not match the successful fenced Worker Job",
        ))
    }
}

fn seal_git_snapshot(
    snapshot: &ValidatedGitSnapshotFact,
) -> Result<[u8; 32], DeliveryValidationError> {
    let identity = GitSnapshotSealIdentity {
        stage_run_id: &snapshot.stage_run_id,
        session_binding_id: &snapshot.session_binding_id,
        product_session_id: &snapshot.product_session_id,
        execution_job_id: &snapshot.execution_job_id,
        attempt: snapshot.attempt,
        lease_id: &snapshot.lease_id,
        fencing_token: &snapshot.fencing_token,
        worker_id: &snapshot.worker_id,
        worker_instance_id: &snapshot.worker_instance_id,
        worker_session_id: &snapshot.worker_session_id,
        codex_thread_id: &snapshot.codex_thread_id,
        repository: &snapshot.repository,
        base_commit_id: &snapshot.base_commit_id,
        base_tree_id: &snapshot.base_tree_id,
        candidate_commit_id: &snapshot.candidate_commit_id,
        candidate_tree_id: &snapshot.candidate_tree_id,
        diff_sha256: &snapshot.diff_sha256,
        changed_paths: &snapshot.changed_paths,
        changed_hunks: &snapshot.changed_hunks,
        artifact_ref: &snapshot.artifact_ref,
        artifact_digest: &snapshot.artifact_digest,
        last_event_sequence: snapshot.last_event_sequence,
        finished_at_millis: snapshot.finished_at_millis,
    };
    let encoded = serde_json::to_vec(&identity).map_err(|error| {
        invalid_candidate(&format!("Git snapshot seal cannot be encoded: {error}"))
    })?;
    Ok(Sha256::digest(encoded).into())
}

fn git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn portable_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PATH_LENGTH
        && !value.starts_with('/')
        && !value.contains('\\')
        && !value.bytes().any(|byte| byte <= 31 || byte == 127)
        && !value
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
        && !value
            .get(1..2)
            .is_some_and(|second| second == ":" && value.as_bytes()[0].is_ascii_alphabetic())
}

fn invalid_candidate(message: &str) -> DeliveryValidationError {
    validation_error(
        DeliveryValidationErrorCode::InvalidValue,
        "candidate",
        message,
    )
}

fn stale_candidate(message: &str) -> DeliveryValidationError {
    validation_error(
        DeliveryValidationErrorCode::RelationshipMismatch,
        "candidate",
        message,
    )
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support {
    use super::*;
    use crate::application::stage::{
        ActiveLeaseIdentity, TerminalArtifactReference, TerminalOutcomeMetadata,
        TerminalOutcomeStatus, fixture_verified_terminal_outcome,
    };
    use winwincode_domain::{ArtifactId, ExecutionAckSequence, Sha256Digest};

    pub(crate) fn validated_git_snapshot(
        delivery: &Delivery,
        stage_run_id: &StageRunId,
        session_binding_id: &SessionBindingId,
        candidate_commit_id: &str,
        candidate_tree_id: &str,
        diff_sha256: &str,
        changed_paths: Vec<CandidatePathFact>,
    ) -> ValidatedGitSnapshotFact {
        let base_commit_id = if git_object_id(&delivery.snapshot().spec.base_revision) {
            delivery.snapshot().spec.base_revision.clone()
        } else {
            "0123456789012345678901234567890123456789".into()
        };
        validated_git_snapshot_between(
            delivery,
            stage_run_id,
            session_binding_id,
            &base_commit_id,
            "1111111111111111111111111111111111111111",
            candidate_commit_id,
            candidate_tree_id,
            diff_sha256,
            changed_paths,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn validated_git_snapshot_between(
        delivery: &Delivery,
        stage_run_id: &StageRunId,
        session_binding_id: &SessionBindingId,
        base_commit_id: &str,
        base_tree_id: &str,
        candidate_commit_id: &str,
        candidate_tree_id: &str,
        diff_sha256: &str,
        changed_paths: Vec<CandidatePathFact>,
    ) -> ValidatedGitSnapshotFact {
        let run = delivery
            .snapshot()
            .stage_runs
            .iter()
            .find(|run| &run.id == stage_run_id)
            .expect("fixture StageRun");
        let binding = delivery
            .snapshot()
            .session_bindings
            .iter()
            .find(|binding| &binding.id == session_binding_id)
            .expect("fixture SessionBinding");
        let worker_session_id = binding
            .worker_session_id
            .clone()
            .expect("fixture WorkerSession");
        let codex_thread_id = binding
            .codex_thread_id
            .clone()
            .expect("fixture CodexThread");
        let mut fact = ValidatedGitSnapshotFact {
            stage_run_id: run.id.clone(),
            session_binding_id: binding.id.clone(),
            product_session_id: binding.product_session_id.clone(),
            execution_job_id: binding.execution_job_id.clone(),
            attempt: run.attempt,
            lease_id: LeaseId(format!("lease-{}", run.id.0)),
            fencing_token: FencingToken("1".into()),
            worker_id: WorkerId("worker-candidate".into()),
            worker_instance_id: WorkerInstanceId("worker-instance-candidate".into()),
            worker_session_id,
            codex_thread_id,
            repository: delivery.snapshot().spec.repository.clone(),
            base_commit_id: base_commit_id.into(),
            base_tree_id: base_tree_id.into(),
            candidate_commit_id: candidate_commit_id.into(),
            candidate_tree_id: candidate_tree_id.into(),
            diff_sha256: diff_sha256.into(),
            changed_hunks: changed_paths
                .iter()
                .map(|path| CandidateHunkFact {
                    file_path: path.path.clone(),
                    hunk_sha256: "b".repeat(64),
                    source_hunk_sha256: None,
                })
                .collect(),
            changed_paths,
            artifact_ref: format!("artifact:job:{}", binding.execution_job_id.0),
            artifact_digest: Sha256Digest(format!("sha256:{}", "9".repeat(64))),
            last_event_sequence: 12,
            finished_at_millis: run.finished_at_millis.expect("fixture finished StageRun"),
            validation_seal: [0; 32],
        };
        fact.validation_seal = seal_git_snapshot(&fact).expect("fixture Git snapshot seal");
        fact
    }

    pub(crate) fn with_changed_hunks(
        mut fact: ValidatedGitSnapshotFact,
        changed_hunks: Vec<CandidateHunkFact>,
    ) -> ValidatedGitSnapshotFact {
        fact.changed_hunks = changed_hunks;
        fact.validation_seal = seal_git_snapshot(&fact).expect("fixture Git snapshot seal");
        fact
    }

    pub(crate) fn with_foreign_terminal_workspace(
        mut fact: ValidatedGitSnapshotFact,
    ) -> ValidatedGitSnapshotFact {
        fact.lease_id = LeaseId("lease-foreign".into());
        fact.fencing_token = FencingToken("2".into());
        fact.worker_id = WorkerId("worker-foreign".into());
        fact.worker_instance_id = WorkerInstanceId("worker-instance-foreign".into());
        fact.artifact_ref = "artifact:job:foreign".into();
        fact.artifact_digest = Sha256Digest(format!("sha256:{}", "8".repeat(64)));
        fact.last_event_sequence += 1;
        fact.finished_at_millis += 1;
        fact.validation_seal = seal_git_snapshot(&fact).expect("fixture Git snapshot seal");
        fact
    }

    pub(crate) fn freeze_facts(
        delivery: &Delivery,
        snapshot: ValidatedGitSnapshotFact,
    ) -> FreezeCandidateFacts {
        let outcome = fixture_verified_terminal_outcome(
            snapshot.stage_run_id.clone(),
            ActiveLeaseIdentity {
                execution_job_id: snapshot.execution_job_id.clone(),
                attempt: snapshot.attempt,
                lease_id: snapshot.lease_id.clone(),
                fencing_token: snapshot.fencing_token.clone(),
                worker_id: snapshot.worker_id.clone(),
                worker_instance_id: snapshot.worker_instance_id.clone(),
                worker_session_id: snapshot.worker_session_id.clone(),
            },
            TerminalOutcomeStatus::Succeeded,
            TerminalOutcomeMetadata {
                codex_thread_id: Some(snapshot.codex_thread_id.clone()),
                finished_at_millis: snapshot.finished_at_millis,
                last_event_sequence: ExecutionAckSequence(
                    i64::try_from(snapshot.last_event_sequence)
                        .expect("fixture event sequence fits i64"),
                ),
                artifacts: vec![TerminalArtifactReference {
                    artifact_id: ArtifactId(snapshot.artifact_ref.clone()),
                    digest: snapshot.artifact_digest.clone(),
                }],
            },
        );
        let _ = delivery;
        FreezeCandidateFacts {
            git_snapshot: snapshot,
            terminal_outcome: outcome,
        }
    }

    pub(crate) fn frozen_candidate(
        delivery: &Delivery,
        stage_run_id: &StageRunId,
        session_binding_id: &SessionBindingId,
    ) -> FrozenDeliveryCandidate {
        let snapshot = validated_git_snapshot(
            delivery,
            stage_run_id,
            session_binding_id,
            "2222222222222222222222222222222222222222",
            "3333333333333333333333333333333333333333",
            &"a".repeat(64),
            vec![CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("4444444444444444444444444444444444444444".into()),
            }],
        );
        freeze_delivery_candidate(delivery, &freeze_facts(delivery, snapshot))
            .expect("fixture frozen candidate")
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{freeze_facts, validated_git_snapshot};
    use super::*;
    use crate::domain::{DeliveryStatus, test_fixture};
    use winwincode_domain::{CodexThreadId, ExecutionJobId, ProductSessionId, WorkerSessionId};

    fn writer_delivery() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        snapshot.evidence.clear();
        snapshot.verdict = None;
        let run = &mut snapshot.stage_runs[0];
        run.id = StageRunId("stage-executor-1".into());
        run.stage = DeliveryStage::Executing;
        run.role = "executor".into();
        run.status = StageRunStatus::Succeeded;
        run.started_at_millis = 1_800_000_000_010;
        run.finished_at_millis = Some(1_800_000_000_020);
        let binding = &mut snapshot.session_bindings[0];
        binding.id = SessionBindingId("binding-executor-1".into());
        binding.stage_run_id = run.id.clone();
        binding.product_session_id = ProductSessionId("product-executor".into());
        binding.execution_job_id = ExecutionJobId("job-executor".into());
        binding.worker_session_id = Some(WorkerSessionId("worker-executor".into()));
        binding.codex_thread_id = Some(CodexThreadId("thread-executor".into()));
        binding.bound_at_millis = 1_800_000_000_011;
        Delivery::try_from_snapshot(snapshot).expect("writer Delivery")
    }

    fn snapshot(delivery: &Delivery) -> ValidatedGitSnapshotFact {
        validated_git_snapshot(
            delivery,
            &StageRunId("stage-executor-1".into()),
            &SessionBindingId("binding-executor-1".into()),
            "2222222222222222222222222222222222222222",
            "3333333333333333333333333333333333333333",
            &"a".repeat(64),
            vec![CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("4444444444444444444444444444444444444444".into()),
            }],
        )
    }

    #[test]
    fn freezes_candidate_from_exact_writer_facts() {
        let delivery = writer_delivery();
        let first =
            freeze_delivery_candidate(&delivery, &freeze_facts(&delivery, snapshot(&delivery)))
                .expect("candidate");
        let second =
            freeze_delivery_candidate(&delivery, &freeze_facts(&delivery, snapshot(&delivery)))
                .expect("candidate");
        assert_eq!(first, second);
        assert_eq!(
            first.candidate_ref().len(),
            "git-candidate:sha256:".len() + 64
        );
        assert_frozen_candidate_current(&delivery, &first).expect("current");

        let mut rebound = delivery.clone().into_snapshot();
        rebound.session_bindings[0].product_session_id =
            ProductSessionId("product-executor-rebound".into());
        rebound.session_bindings[0].execution_job_id =
            ExecutionJobId("job-executor-rebound".into());
        let rebound = Delivery::try_from_snapshot(rebound).expect("rebound writer");
        let rebound_candidate =
            freeze_delivery_candidate(&rebound, &freeze_facts(&rebound, snapshot(&rebound)))
                .expect("rebound candidate");
        assert_ne!(first.candidate_ref(), rebound_candidate.candidate_ref());
    }

    #[test]
    fn candidate_requires_sealed_validated_git_snapshot() {
        let delivery = writer_delivery();
        let mut modified_after_validation = snapshot(&delivery);
        modified_after_validation.candidate_tree_id = "5".repeat(40);
        assert!(
            freeze_delivery_candidate(
                &delivery,
                &freeze_facts(&delivery, modified_after_validation)
            )
            .is_err()
        );
    }

    #[test]
    fn candidate_requires_sealed_matching_worker_job_snapshot() {
        let delivery = writer_delivery();
        let current = snapshot(&delivery);
        let mut other_job_snapshot = current.clone();
        other_job_snapshot.execution_job_id = ExecutionJobId("job-foreign".into());
        other_job_snapshot.artifact_ref = "artifact:job:job-foreign".into();
        other_job_snapshot.validation_seal =
            seal_git_snapshot(&other_job_snapshot).expect("other sealed Job snapshot");
        let current_outcome = freeze_facts(&delivery, current).terminal_outcome;
        assert!(
            freeze_delivery_candidate(
                &delivery,
                &FreezeCandidateFacts {
                    git_snapshot: other_job_snapshot,
                    terminal_outcome: current_outcome,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn candidate_rejects_snapshot_metadata_not_named_by_the_accepted_outcome() {
        let delivery = writer_delivery();
        let original = snapshot(&delivery);
        let accepted_outcome = freeze_facts(&delivery, original.clone()).terminal_outcome;
        let mut substituted_artifact = original;
        substituted_artifact.artifact_ref = "artifact:job:foreign".into();
        substituted_artifact.artifact_digest = Sha256Digest(format!("sha256:{}", "8".repeat(64)));
        substituted_artifact.last_event_sequence += 1;
        substituted_artifact.validation_seal =
            seal_git_snapshot(&substituted_artifact).expect("substituted sealed artifact");

        assert!(
            freeze_delivery_candidate(
                &delivery,
                &FreezeCandidateFacts {
                    git_snapshot: substituted_artifact,
                    terminal_outcome: accepted_outcome,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn candidate_requires_exactly_one_writer_session_binding() {
        let delivery = writer_delivery();
        let facts = freeze_facts(&delivery, snapshot(&delivery));
        let mut ambiguous = delivery.into_snapshot();
        ambiguous.session_bindings.push(SessionBinding {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId("binding-executor-duplicate".into()),
            delivery_id: ambiguous.id.clone(),
            delivery_task_id: ambiguous.stage_runs[0].delivery_task_id.clone(),
            stage_run_id: StageRunId("stage-executor-1".into()),
            product_session_id: ProductSessionId("product-executor-duplicate".into()),
            execution_job_id: ExecutionJobId("job-executor-duplicate".into()),
            worker_session_id: Some(WorkerSessionId("worker-executor-duplicate".into())),
            codex_thread_id: Some(CodexThreadId("thread-executor-duplicate".into())),
            bound_at_millis: 1_800_000_000_012,
        });
        let ambiguous = Delivery::try_from_snapshot(ambiguous)
            .expect("aggregate permits verification of writer binding cardinality here");

        assert!(freeze_delivery_candidate(&ambiguous, &facts).is_err());
    }

    #[test]
    fn candidate_rejects_taskless_executor_or_remediator_writer() {
        for (stage, role) in [
            (DeliveryStage::Executing, "executor"),
            (DeliveryStage::Reworking, "remediator"),
        ] {
            let mut taskless = writer_delivery().into_snapshot();
            taskless.stage_runs[0].stage = stage;
            taskless.stage_runs[0].role = role.into();
            taskless.stage_runs[0].delivery_task_id = None;
            taskless.session_bindings[0].delivery_task_id = None;
            let taskless = Delivery::try_from_snapshot(taskless)
                .expect("aggregate permits a Delivery-level run that cannot produce a candidate");
            let facts = freeze_facts(&taskless, snapshot(&taskless));

            freeze_candidate_for_stage(&taskless, &facts, stage)
                .expect_err("a candidate writer must bind one concrete DeliveryTask");
        }
    }

    #[test]
    fn generic_candidate_freeze_rejects_a_remediator_writer() {
        let mut replacement = writer_delivery().into_snapshot();
        replacement.stage_runs[0].stage = DeliveryStage::Reworking;
        replacement.stage_runs[0].role = "remediator".into();
        let replacement = Delivery::try_from_snapshot(replacement).expect("remediator output");
        let facts = freeze_facts(&replacement, snapshot(&replacement));

        freeze_delivery_candidate(&replacement, &facts)
            .expect_err("remediator output requires the authorization-bound replacement entry");
    }

    #[test]
    fn candidate_rejects_an_ambiguous_same_time_writer() {
        let delivery = writer_delivery();
        let facts = freeze_facts(&delivery, snapshot(&delivery));
        let mut ambiguous = delivery.into_snapshot();
        let task_id = ambiguous.stage_runs[0].delivery_task_id.clone();
        ambiguous.stage_runs.insert(
            0,
            StageRun {
                schema_version: super::super::DELIVERY_SCHEMA_VERSION,
                id: StageRunId("stage-executor-concurrent".into()),
                delivery_id: ambiguous.id.clone(),
                delivery_task_id: task_id.clone(),
                stage: DeliveryStage::Executing,
                actor_type: StageRunActorType::Codex,
                role: "executor".into(),
                status: StageRunStatus::Succeeded,
                attempt: 1,
                started_at_millis: 1_800_000_000_010,
                finished_at_millis: Some(1_800_000_000_020),
            },
        );
        ambiguous.session_bindings.push(SessionBinding {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId("binding-executor-concurrent".into()),
            delivery_id: ambiguous.id.clone(),
            delivery_task_id: task_id,
            stage_run_id: StageRunId("stage-executor-concurrent".into()),
            product_session_id: ProductSessionId("product-executor-concurrent".into()),
            execution_job_id: ExecutionJobId("job-executor-concurrent".into()),
            worker_session_id: Some(WorkerSessionId("worker-executor-concurrent".into())),
            codex_thread_id: Some(CodexThreadId("thread-executor-concurrent".into())),
            bound_at_millis: 1_800_000_000_011,
        });
        let ambiguous = Delivery::try_from_snapshot(ambiguous)
            .expect("aggregate permits candidate writer ambiguity check here");

        assert!(freeze_delivery_candidate(&ambiguous, &facts).is_err());
    }

    #[test]
    fn any_later_writer_stage_invalidates_the_candidate() {
        let delivery = writer_delivery();
        let candidate =
            freeze_delivery_candidate(&delivery, &freeze_facts(&delivery, snapshot(&delivery)))
                .expect("candidate");
        let mut later = delivery.into_snapshot();
        later.stage_runs.push(StageRun {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: StageRunId("stage-human-writer".into()),
            delivery_id: later.id.clone(),
            delivery_task_id: later.stage_runs[0].delivery_task_id.clone(),
            stage: DeliveryStage::Executing,
            actor_type: StageRunActorType::Human,
            role: "executor".into(),
            status: StageRunStatus::Running,
            attempt: 2,
            started_at_millis: 1_800_000_000_030,
            finished_at_millis: None,
        });
        later.updated_at_millis = 1_800_000_000_030;
        let later = Delivery::try_from_snapshot(later)
            .expect("aggregate permits candidate fail-closed writer detection here");

        assert!(assert_frozen_candidate_current(&later, &candidate).is_err());
    }

    #[test]
    fn rejects_candidate_after_spec_or_writer_change() {
        let delivery = writer_delivery();
        let candidate =
            freeze_delivery_candidate(&delivery, &freeze_facts(&delivery, snapshot(&delivery)))
                .expect("candidate");

        let mut changed_spec = delivery.clone().into_snapshot();
        changed_spec.spec.revision += 1;
        changed_spec.revision += 1;
        let changed_spec = Delivery::try_from_snapshot(changed_spec).expect("new spec");
        assert!(assert_frozen_candidate_current(&changed_spec, &candidate).is_err());

        let mut later = delivery.into_snapshot();
        later.stage_runs.push(StageRun {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: StageRunId("stage-remediator-1".into()),
            delivery_id: later.id.clone(),
            delivery_task_id: later.stage_runs[0].delivery_task_id.clone(),
            stage: DeliveryStage::Reworking,
            actor_type: StageRunActorType::Codex,
            role: "remediator".into(),
            status: StageRunStatus::Running,
            attempt: 1,
            started_at_millis: 1_800_000_000_030,
            finished_at_millis: None,
        });
        later.session_bindings.push(SessionBinding {
            schema_version: super::super::DELIVERY_SCHEMA_VERSION,
            id: SessionBindingId("binding-remediator-1".into()),
            delivery_id: later.id.clone(),
            delivery_task_id: later.stage_runs[0].delivery_task_id.clone(),
            stage_run_id: StageRunId("stage-remediator-1".into()),
            product_session_id: ProductSessionId("product-remediator".into()),
            execution_job_id: ExecutionJobId("job-remediator".into()),
            worker_session_id: Some(WorkerSessionId("worker-remediator".into())),
            codex_thread_id: Some(CodexThreadId("thread-remediator".into())),
            bound_at_millis: 1_800_000_000_031,
        });
        later.updated_at_millis = 1_800_000_000_031;
        later.revision += 1;
        let later = Delivery::try_from_snapshot(later).expect("later writer");
        assert!(assert_frozen_candidate_current(&later, &candidate).is_err());
    }
}
