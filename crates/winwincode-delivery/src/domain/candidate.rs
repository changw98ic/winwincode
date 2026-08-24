// SPDX-License-Identifier: Apache-2.0

//! Deterministic, rebuildable Git candidate identity.
//!
//! A candidate is deliberately not part of [`super::DeliverySnapshot`]. The
//! Control Plane rebuilds it from the current Delivery writer and exact Git
//! facts, then persists only candidate references in Evidence and Verdict.

use std::collections::HashSet;

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{
    Delivery, DeliveryStage, DeliveryValidationError, DeliveryValidationErrorCode, RepositoryRef,
    SessionBinding, SessionBindingId, StageRun, StageRunActorType, StageRunStatus,
    validation_error,
};
use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, FencingToken, LeaseId,
    ProductSessionId, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
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

/// Exact Git and writer identities observed after a writer succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreezeCandidateFacts {
    pub producer_stage_run_id: StageRunId,
    pub producer_session_binding_id: SessionBindingId,
    pub base_commit_id: String,
    pub base_tree_id: String,
    pub candidate_commit_id: String,
    pub candidate_tree_id: String,
    /// Lowercase SHA-256 hex without a prefix, matching Git diff tooling.
    pub diff_sha256: String,
    pub changed_paths: Vec<CandidatePathFact>,
}

/// Commit/tree pair read from the authoritative Git object database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGitCommit {
    pub commit_id: String,
    pub tree_id: String,
}

/// Base-to-candidate diff read from the authoritative Git object database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGitDiff {
    pub base_commit_id: String,
    pub candidate_commit_id: String,
    pub diff_sha256: String,
    pub changed_paths: Vec<CandidatePathFact>,
}

/// Accepted terminal outcome used to locate the producer Job workspace.
///
/// This value comes from the trusted Control Plane ledger adapter. It is not a
/// browser or Worker command payload and it never decides a Delivery verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedCandidateProducerOutcome {
    pub stage_run_id: StageRunId,
    pub execution_job_id: ExecutionJobId,
    pub attempt: u64,
    pub lease_id: LeaseId,
    pub fencing_token: FencingToken,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: WorkerSessionId,
    pub codex_thread_id: CodexThreadId,
    pub succeeded: bool,
    pub finished_at_millis: u64,
}

/// Trusted Git snapshot boundary implemented by the local/remote Worker adapter.
///
/// Browser and command payloads never implement this port. The Control Plane
/// wires an adapter that resolves immutable Git objects independently from the
/// values in [`FreezeCandidateFacts`].
pub trait CandidateGitSnapshotResolver {
    fn accepted_successful_outcome(
        &self,
        stage_run: &StageRun,
        binding: &SessionBinding,
    ) -> Result<AcceptedCandidateProducerOutcome, String>;

    fn resolve_commit(
        &self,
        producer: &AcceptedCandidateProducerOutcome,
        repository: &RepositoryRef,
        commit_id: &str,
    ) -> Result<ResolvedGitCommit, String>;

    fn resolve_diff(
        &self,
        producer: &AcceptedCandidateProducerOutcome,
        repository: &RepositoryRef,
        base_commit_id: &str,
        candidate_commit_id: &str,
    ) -> Result<ResolvedGitDiff, String>;
}

/// One immutable candidate identity derived from canonical Delivery and Git facts.
///
/// This value is serializable for projections, but it is not deserializable and
/// cannot be inserted as an eleventh persisted Delivery object.
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

    pub fn delivery_spec_revision(&self) -> u64 {
        self.delivery_spec_revision
    }

    pub fn repository(&self) -> &RepositoryRef {
        &self.repository
    }

    pub fn base_revision(&self) -> &str {
        &self.base_revision
    }

    pub fn producer_stage_run_id(&self) -> &StageRunId {
        &self.producer_stage_run_id
    }

    pub fn producer_delivery_task_id(&self) -> Option<&DeliveryTaskId> {
        self.producer_delivery_task_id.as_ref()
    }

    pub fn producer_stage(&self) -> DeliveryStage {
        self.producer_stage
    }

    pub fn producer_role(&self) -> &str {
        &self.producer_role
    }

    pub fn producer_attempt(&self) -> u64 {
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
    base_commit_id: &'candidate str,
    base_tree_id: &'candidate str,
    candidate_commit_id: &'candidate str,
    candidate_tree_id: &'candidate str,
    diff_sha256: &'candidate str,
    changed_paths: &'candidate [CandidatePathFact],
}

/// Freezes exact Git facts against the latest successful Delivery writer.
///
/// # Errors
///
/// Rejects malformed Git facts, a non-current writer, an incomplete producer
/// SessionBinding, or a producer outside the executor/remediator policy.
pub fn freeze_delivery_candidate(
    delivery: &Delivery,
    mut facts: FreezeCandidateFacts,
    git: &impl CandidateGitSnapshotResolver,
) -> Result<FrozenDeliveryCandidate, DeliveryValidationError> {
    let producer = current_writer(delivery, &facts.producer_stage_run_id)?;
    let binding = exact_producer_binding(delivery, producer, &facts.producer_session_binding_id)?;

    let mut changed_paths = std::mem::take(&mut facts.changed_paths);
    validate_git_facts(delivery, &facts, &mut changed_paths)?;
    let outcome = verify_authoritative_producer_outcome(producer, binding, git)?;
    verify_authoritative_git_snapshot(delivery, &facts, &changed_paths, &outcome, git)?;
    let worker_session_id = binding
        .worker_session_id
        .clone()
        .ok_or_else(|| stale_candidate("candidate producer WorkerSession is missing"))?;
    let codex_thread_id = binding
        .codex_thread_id
        .clone()
        .ok_or_else(|| stale_candidate("candidate producer CodexThread is missing"))?;

    let mut candidate = FrozenDeliveryCandidate {
        candidate_ref: String::new(),
        delivery_id: delivery.id().clone(),
        delivery_spec_id: delivery.snapshot().spec.id.clone(),
        delivery_spec_revision: delivery.snapshot().spec.revision,
        repository: delivery.snapshot().spec.repository.clone(),
        base_revision: delivery.snapshot().spec.base_revision.clone(),
        producer_delivery_task_id: producer.delivery_task_id.clone(),
        producer_stage_run_id: facts.producer_stage_run_id,
        producer_stage: producer.stage,
        producer_role: producer.role.clone(),
        producer_attempt: producer.attempt,
        producer_session_binding_id: facts.producer_session_binding_id,
        producer_product_session_id: binding.product_session_id.clone(),
        producer_execution_job_id: binding.execution_job_id.clone(),
        producer_worker_session_id: worker_session_id,
        producer_codex_thread_id: codex_thread_id,
        base_commit_id: facts.base_commit_id,
        base_tree_id: facts.base_tree_id,
        candidate_commit_id: facts.candidate_commit_id,
        candidate_tree_id: facts.candidate_tree_id,
        diff_sha256: facts.diff_sha256,
        changed_paths,
    };
    let identity = CandidateIdentity::from(&candidate);
    let encoded = serde_json::to_vec(&identity).map_err(|error| {
        validation_error(
            DeliveryValidationErrorCode::InvalidValue,
            "candidate",
            format!("candidate identity cannot be encoded: {error}"),
        )
    })?;
    candidate.candidate_ref = format!("git-candidate:sha256:{:x}", Sha256::digest(encoded));
    Ok(candidate)
}

/// Rebuilds a frozen candidate and rejects stale or modified derived facts.
///
/// # Errors
///
/// Rejects a candidate after its Spec changes, its facts change, or any later
/// executor/remediator writer starts.
pub fn assert_frozen_candidate_current(
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
        && binding.codex_thread_id.as_ref() == Some(&candidate.producer_codex_thread_id);
    if !same_writer {
        return Err(stale_candidate(
            "candidate writer or complete SessionBinding identity changed",
        ));
    }
    let identity = CandidateIdentity::from(candidate);
    let encoded = serde_json::to_vec(&identity).map_err(|_| {
        stale_candidate("candidate identity can no longer be encoded deterministically")
    })?;
    let expected_ref = format!("git-candidate:sha256:{:x}", Sha256::digest(encoded));
    if candidate.candidate_ref != expected_ref {
        return Err(stale_candidate("candidate facts changed after freezing"));
    }
    Ok(())
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
        || !valid_role
    {
        return Err(stale_candidate(
            "candidate producer must be one successful Codex executor or remediator",
        ));
    }
    let later_writer = runs.iter().enumerate().any(|(index, run)| {
        run.id != producer.id
            && run.actor_type == StageRunActorType::Codex
            && matches!(
                run.stage,
                DeliveryStage::Executing | DeliveryStage::Reworking
            )
            && (index > producer_index
                || run.started_at_millis > producer.started_at_millis
                || (run.started_at_millis == producer.started_at_millis
                    && run.attempt > producer.attempt))
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
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| &binding.id == binding_id)
        .ok_or_else(|| stale_candidate("candidate producer SessionBinding is missing"))?;
    let complete = binding.worker_session_id.is_some() && binding.codex_thread_id.is_some();
    if binding.delivery_id != *delivery.id()
        || binding.stage_run_id != producer.id
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

fn validate_git_facts(
    delivery: &Delivery,
    facts: &FreezeCandidateFacts,
    changed_paths: &mut Vec<CandidatePathFact>,
) -> Result<(), DeliveryValidationError> {
    let object_ids = [
        facts.base_commit_id.as_str(),
        facts.base_tree_id.as_str(),
        facts.candidate_commit_id.as_str(),
        facts.candidate_tree_id.as_str(),
    ];
    if object_ids.iter().any(|value| !git_object_id(value)) || !lowercase_sha256(&facts.diff_sha256)
    {
        return Err(invalid_candidate(
            "candidate Git objects and diff digest must be lowercase hexadecimal identities",
        ));
    }
    let object_length = object_ids[0].len();
    if object_ids.iter().any(|value| value.len() != object_length) {
        return Err(invalid_candidate(
            "candidate Git object identities must use one repository object format",
        ));
    }
    if git_object_id(&delivery.snapshot().spec.base_revision)
        && delivery.snapshot().spec.base_revision != facts.base_commit_id
    {
        return Err(invalid_candidate(
            "candidate base commit does not match DeliverySpec.baseRevision",
        ));
    }
    if changed_paths.len() > MAX_CHANGED_PATHS {
        return Err(invalid_candidate(
            "candidate changed paths exceed the supported limit",
        ));
    }
    changed_paths.sort_by(|left, right| left.path.cmp(&right.path));
    let mut paths = HashSet::with_capacity(changed_paths.len());
    for fact in changed_paths {
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
    Ok(())
}

fn verify_authoritative_git_snapshot(
    delivery: &Delivery,
    facts: &FreezeCandidateFacts,
    changed_paths: &[CandidatePathFact],
    outcome: &AcceptedCandidateProducerOutcome,
    git: &impl CandidateGitSnapshotResolver,
) -> Result<(), DeliveryValidationError> {
    let repository = &delivery.snapshot().spec.repository;
    let base = git
        .resolve_commit(outcome, repository, &facts.base_commit_id)
        .map_err(|_| invalid_candidate("authoritative Git resolver rejected the base commit"))?;
    let candidate = git
        .resolve_commit(outcome, repository, &facts.candidate_commit_id)
        .map_err(|_| {
            invalid_candidate("authoritative Git resolver rejected the candidate commit")
        })?;
    let mut diff = git
        .resolve_diff(
            outcome,
            repository,
            &facts.base_commit_id,
            &facts.candidate_commit_id,
        )
        .map_err(|_| invalid_candidate("authoritative Git resolver rejected the candidate diff"))?;
    diff.changed_paths
        .sort_by(|left, right| left.path.cmp(&right.path));
    let exact = base.commit_id == facts.base_commit_id
        && base.tree_id == facts.base_tree_id
        && candidate.commit_id == facts.candidate_commit_id
        && candidate.tree_id == facts.candidate_tree_id
        && diff.base_commit_id == facts.base_commit_id
        && diff.candidate_commit_id == facts.candidate_commit_id
        && diff.diff_sha256 == facts.diff_sha256
        && diff.changed_paths == changed_paths;
    if exact {
        Ok(())
    } else {
        Err(invalid_candidate(
            "candidate facts do not match the authoritative Git commit, tree, diff, and path snapshot",
        ))
    }
}

fn verify_authoritative_producer_outcome(
    producer: &StageRun,
    binding: &SessionBinding,
    git: &impl CandidateGitSnapshotResolver,
) -> Result<AcceptedCandidateProducerOutcome, DeliveryValidationError> {
    let outcome = git
        .accepted_successful_outcome(producer, binding)
        .map_err(|_| {
            stale_candidate("candidate producer has no accepted successful Worker outcome")
        })?;
    let exact = outcome.succeeded
        && outcome.stage_run_id == producer.id
        && outcome.execution_job_id == binding.execution_job_id
        && outcome.attempt == producer.attempt
        && binding.worker_session_id.as_ref() == Some(&outcome.worker_session_id)
        && binding.codex_thread_id.as_ref() == Some(&outcome.codex_thread_id)
        && producer.finished_at_millis == Some(outcome.finished_at_millis)
        && !outcome.lease_id.0.is_empty()
        && !outcome.fencing_token.0.is_empty()
        && !outcome.worker_id.0.is_empty()
        && !outcome.worker_instance_id.0.is_empty();
    if exact {
        Ok(outcome)
    } else {
        Err(stale_candidate(
            "candidate outcome does not match the producer Job, attempt, lease, Worker, sessions, or finish",
        ))
    }
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

#[cfg(test)]
mod tests {
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

    fn facts() -> FreezeCandidateFacts {
        FreezeCandidateFacts {
            producer_stage_run_id: StageRunId("stage-executor-1".into()),
            producer_session_binding_id: SessionBindingId("binding-executor-1".into()),
            base_commit_id: "0123456789012345678901234567890123456789".into(),
            base_tree_id: "1111111111111111111111111111111111111111".into(),
            candidate_commit_id: "2222222222222222222222222222222222222222".into(),
            candidate_tree_id: "3333333333333333333333333333333333333333".into(),
            diff_sha256: "a".repeat(64),
            changed_paths: vec![CandidatePathFact {
                path: "src/invitation.rs".into(),
                state: CandidatePathState::Present,
                object_id: Some("4444444444444444444444444444444444444444".into()),
            }],
        }
    }

    struct GitFixture {
        facts: FreezeCandidateFacts,
    }

    impl CandidateGitSnapshotResolver for GitFixture {
        fn accepted_successful_outcome(
            &self,
            stage_run: &StageRun,
            binding: &SessionBinding,
        ) -> Result<AcceptedCandidateProducerOutcome, String> {
            Ok(AcceptedCandidateProducerOutcome {
                stage_run_id: stage_run.id.clone(),
                execution_job_id: binding.execution_job_id.clone(),
                attempt: stage_run.attempt,
                lease_id: LeaseId("lease-candidate".into()),
                fencing_token: FencingToken("1".into()),
                worker_id: WorkerId("worker-candidate".into()),
                worker_instance_id: WorkerInstanceId("worker-instance-candidate".into()),
                worker_session_id: binding.worker_session_id.clone().expect("worker"),
                codex_thread_id: binding.codex_thread_id.clone().expect("thread"),
                succeeded: true,
                finished_at_millis: stage_run.finished_at_millis.expect("finished"),
            })
        }

        fn resolve_commit(
            &self,
            _producer: &AcceptedCandidateProducerOutcome,
            _repository: &RepositoryRef,
            commit_id: &str,
        ) -> Result<ResolvedGitCommit, String> {
            if commit_id == self.facts.base_commit_id {
                Ok(ResolvedGitCommit {
                    commit_id: commit_id.into(),
                    tree_id: self.facts.base_tree_id.clone(),
                })
            } else if commit_id == self.facts.candidate_commit_id {
                Ok(ResolvedGitCommit {
                    commit_id: commit_id.into(),
                    tree_id: self.facts.candidate_tree_id.clone(),
                })
            } else {
                Err("unknown commit".into())
            }
        }

        fn resolve_diff(
            &self,
            _producer: &AcceptedCandidateProducerOutcome,
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

    #[test]
    fn freezes_candidate_from_exact_writer_facts() {
        let delivery = writer_delivery();
        let git = GitFixture { facts: facts() };
        let first = freeze_delivery_candidate(&delivery, facts(), &git).expect("candidate");
        let second = freeze_delivery_candidate(&delivery, facts(), &git).expect("candidate");
        assert_eq!(first, second);
        assert_eq!(
            first.candidate_ref().len(),
            "git-candidate:sha256:".len() + 64
        );
        assert_frozen_candidate_current(&delivery, &first).expect("current");

        let mut rebound = delivery.into_snapshot();
        rebound.session_bindings[0].product_session_id =
            ProductSessionId("product-executor-rebound".into());
        rebound.session_bindings[0].execution_job_id =
            ExecutionJobId("job-executor-rebound".into());
        let rebound = Delivery::try_from_snapshot(rebound).expect("rebound writer");
        let rebound_candidate =
            freeze_delivery_candidate(&rebound, facts(), &git).expect("rebound candidate");
        assert_ne!(first.candidate_ref(), rebound_candidate.candidate_ref());
    }

    #[test]
    fn rejects_candidate_after_spec_or_writer_change() {
        let delivery = writer_delivery();
        let git = GitFixture { facts: facts() };
        let candidate = freeze_delivery_candidate(&delivery, facts(), &git).expect("candidate");

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

    #[test]
    fn rejects_unverified_or_inconsistent_git_snapshot() {
        let delivery = writer_delivery();
        let mut authoritative = facts();
        authoritative.candidate_tree_id = "5".repeat(40);
        let git = GitFixture {
            facts: authoritative,
        };
        assert!(freeze_delivery_candidate(&delivery, facts(), &git).is_err());
    }

    struct MissingOutcome;

    impl CandidateGitSnapshotResolver for MissingOutcome {
        fn accepted_successful_outcome(
            &self,
            _stage_run: &StageRun,
            _binding: &SessionBinding,
        ) -> Result<AcceptedCandidateProducerOutcome, String> {
            Err("missing accepted outcome".into())
        }

        fn resolve_commit(
            &self,
            _producer: &AcceptedCandidateProducerOutcome,
            _repository: &RepositoryRef,
            _commit_id: &str,
        ) -> Result<ResolvedGitCommit, String> {
            Err("unreachable".into())
        }

        fn resolve_diff(
            &self,
            _producer: &AcceptedCandidateProducerOutcome,
            _repository: &RepositoryRef,
            _base_commit_id: &str,
            _candidate_commit_id: &str,
        ) -> Result<ResolvedGitDiff, String> {
            Err("unreachable".into())
        }
    }

    #[test]
    fn rejects_candidate_without_exact_successful_worker_outcome_and_job_snapshot() {
        assert!(freeze_delivery_candidate(&writer_delivery(), facts(), &MissingOutcome).is_err());
    }
}
