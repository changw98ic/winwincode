// SPDX-License-Identifier: Apache-2.0

//! Transactional Snapshot product records owned by the Control Plane seam.

use serde::{Deserialize, Serialize};
use winwincode_domain::{
    Candidate, CandidateId, CanonicalSnapshot, CodexThreadId, ExecutionJobId, ProductSessionId,
    SchemaVersion, Sha256Digest, SnapshotId, WorkRunId, WorkerSessionId, is_canonical_prefixed_id,
    verify_snapshot_seal,
};

use crate::{
    CommitReceipt, NewOutboxEvent, ProductStateStorage, ReceiptIdentity, StateCommit,
    StateMutation, StateRevisionGuard, StorageError,
};

const SNAPSHOT_DISPATCH_TOPIC: &str = "verification.snapshot.dispatch";

/// Runtime-injected binding for one verification execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotVerificationBinding {
    #[serde(rename = "verificationPlan")]
    pub verification_plan: winwincode_domain::VerificationPlan,
    #[serde(rename = "verificationSession")]
    pub verification_session: winwincode_domain::VerificationSession,
    #[serde(rename = "sessionBindingId")]
    pub session_binding_id: String,
    #[serde(rename = "workRunId")]
    pub work_run_id: WorkRunId,
    #[serde(rename = "executionJobId")]
    pub execution_job_id: ExecutionJobId,
    #[serde(rename = "productSessionId")]
    pub product_session_id: ProductSessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "workerSessionId")]
    pub worker_session_id: Option<WorkerSessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "codexThreadId")]
    pub codex_thread_id: Option<CodexThreadId>,
    #[serde(rename = "verificationRole")]
    pub verification_role: String,
    #[serde(rename = "attempt")]
    pub attempt: i64,
}

/// Product values prepared by Control Plane. Storage does not construct
/// Candidate or Snapshot identities from Worker execution facts.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotProductStaged {
    candidate: Candidate,
    snapshot: CanonicalSnapshot,
    binding: SnapshotVerificationBinding,
    state_guards: Vec<StateRevisionGuard>,
}

impl SnapshotProductStaged {
    /// # Errors
    /// Rejects an invalid Candidate, a mismatched or tampered Snapshot, or an
    /// invalid verification binding before any storage write.
    pub fn new(
        candidate: Candidate,
        snapshot: CanonicalSnapshot,
        binding: SnapshotVerificationBinding,
    ) -> Result<Self, StorageError> {
        let staged = Self {
            candidate,
            snapshot,
            binding,
            state_guards: Vec::new(),
        };
        staged.validate()?;
        Ok(staged)
    }

    /// Requires the product authority observed by Control Plane to remain current.
    #[must_use]
    pub fn with_state_guard(mut self, guard: StateRevisionGuard) -> Self {
        self.state_guards.push(guard);
        self
    }

    fn validate(&self) -> Result<(), StorageError> {
        validate_candidate(&self.candidate)?;
        let snapshot = self.snapshot.as_contract();
        if !verify_snapshot_seal(&self.snapshot)
            || snapshot.candidate_id != self.candidate.id
            || snapshot.work_run_id != self.candidate.work_run_id
            || snapshot.base_commit_id.0 != self.candidate.base_commit
            || snapshot.candidate_commit_id.0 != self.candidate.candidate_commit
            || snapshot.candidate_tree_id.0 != self.candidate.candidate_tree
            || snapshot.diff_sha256 != self.candidate.diff_digest
            || self.binding.verification_plan.candidate_digest != self.candidate.candidate_digest
        {
            return Err(StorageError::invalid_input(
                "Snapshot does not match its sealed Candidate",
            ));
        }
        validate_binding(&self.binding)?;
        validate_session_snapshot(&self.binding, &self.snapshot)
    }
}

/// Durable result of one Snapshot product transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotProductCommit {
    pub receipt: CommitReceipt,
    pub candidate: Candidate,
    pub snapshot: CanonicalSnapshot,
    pub binding: SnapshotVerificationBinding,
    pub dispatch: SnapshotVerificationDispatch,
}

/// Internal outbox intent atomically published with a Snapshot product.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotVerificationDispatch {
    pub event_id: String,
    pub topic: String,
    pub snapshot_id: SnapshotId,
    pub candidate_id: CandidateId,
    pub work_run_id: WorkRunId,
    pub execution_job_id: ExecutionJobId,
    pub payload: Vec<u8>,
    pub attempt: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotVerificationDispatchPayload {
    attempt: i64,
    #[serde(rename = "snapshotId")]
    snapshot: SnapshotId,
    #[serde(rename = "candidateId")]
    candidate: CandidateId,
    #[serde(rename = "workRunId")]
    work_run: WorkRunId,
    #[serde(rename = "executionJobId")]
    execution_job: ExecutionJobId,
    #[serde(rename = "sessionBindingId")]
    session_binding: String,
}

/// Exact runtime identities checked before a verification execution/result is
/// accepted against a Snapshot binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotBindingCheck {
    pub snapshot_id: SnapshotId,
    pub execution_job_id: ExecutionJobId,
    pub work_run_id: WorkRunId,
    pub product_session_id: ProductSessionId,
    pub attempt: i64,
}

impl SnapshotBindingCheck {
    #[must_use]
    pub fn new(
        snapshot_id: SnapshotId,
        execution_job_id: ExecutionJobId,
        work_run_id: WorkRunId,
        product_session_id: ProductSessionId,
        attempt: i64,
    ) -> Self {
        Self {
            snapshot_id,
            execution_job_id,
            work_run_id,
            product_session_id,
            attempt,
        }
    }
}

/// Validated Snapshot binding returned at execution and result ingress.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapshotBindingValidated {
    pub verification_plan: winwincode_domain::VerificationPlan,
    pub verification_session: winwincode_domain::VerificationSession,
    pub snapshot_id: SnapshotId,
    pub candidate_id: CandidateId,
    pub execution_job_id: ExecutionJobId,
    pub work_run_id: WorkRunId,
    pub attempt: i64,
}

/// Commits Snapshot, Candidate, verification binding and dispatch intent as
/// one receipt transaction. Exact retries return the original receipt.
///
/// # Errors
/// Returns invalid input or a storage/idempotency/revision failure.
pub fn commit_snapshot_product(
    storage: &mut dyn ProductStateStorage,
    identity: ReceiptIdentity,
    command_digest: Sha256Digest,
    staged: &SnapshotProductStaged,
) -> Result<SnapshotProductCommit, StorageError> {
    staged.validate()?;
    let request_id = identity.request_id().clone();
    let candidate = &staged.candidate;
    let snapshot = &staged.snapshot;
    let candidate_json = serde_json::to_vec(candidate)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let snapshot_json = serde_json::to_vec(snapshot)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let binding_json = serde_json::to_vec(&staged.binding)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let event_id =
        snapshot_dispatch_event_id(&staged.binding.execution_job_id, staged.binding.attempt);
    let dispatch_payload = serde_json::json!({
        "snapshotId": snapshot.snapshot_id(),
        "candidateId": &candidate.id,
        "workRunId": staged.binding.work_run_id,
        "executionJobId": staged.binding.execution_job_id,
        "sessionBindingId": staged.binding.session_binding_id,
        "attempt": staged.binding.attempt,
    });
    let payload = serde_json::to_vec(&dispatch_payload)
        .map_err(|error| StorageError::invalid_input(error.to_string()))?;
    let dispatch = SnapshotVerificationDispatch {
        event_id: event_id.clone(),
        topic: SNAPSHOT_DISPATCH_TOPIC.to_owned(),
        snapshot_id: snapshot.snapshot_id().clone(),
        candidate_id: candidate.id.clone(),
        work_run_id: staged.binding.work_run_id.clone(),
        execution_job_id: staged.binding.execution_job_id.clone(),
        payload: payload.clone(),
        attempt: staged.binding.attempt,
    };
    let mut commit = StateCommit::new(
        identity,
        command_digest,
        binding_stream_id(
            snapshot.snapshot_id(),
            &staged.binding.execution_job_id,
            staged.binding.attempt,
        ),
        0,
        binding_json,
        vec![NewOutboxEvent::internal(
            event_id,
            SNAPSHOT_DISPATCH_TOPIC,
            payload,
        )],
    );
    // A new verification attempt binds the original immutable product. Guard
    // its revision in the same transaction rather than rewriting it.
    for (stream, payload) in [
        (candidate_stream_id(&candidate.id), candidate_json),
        (snapshot_stream_id(snapshot.snapshot_id()), snapshot_json),
    ] {
        commit = match storage.load_state(&stream)? {
            Some(existing) if existing.payload == payload => {
                commit.with_state_guard(StateRevisionGuard::new(stream, existing.revision)?)
            }
            Some(_) => return Err(StorageError::request_conflict(&request_id)),
            None => commit.with_state_mutation(StateMutation::new(stream, 0, payload)?),
        };
    }
    for guard in &staged.state_guards {
        commit = commit.with_state_guard(guard.clone());
    }
    let receipt = storage.commit(&commit)?;
    let durable_candidate = load_durable_product(storage, &candidate_stream_id(&candidate.id))?;
    let durable_snapshot =
        load_durable_product(storage, &snapshot_stream_id(snapshot.snapshot_id()))?;
    let durable_binding = load_durable_product(
        storage,
        &binding_stream_id(
            snapshot.snapshot_id(),
            &staged.binding.execution_job_id,
            staged.binding.attempt,
        ),
    )?;
    let durable_dispatch = dispatch_from_receipt(&receipt)?;
    if &durable_candidate != candidate
        || &durable_snapshot != snapshot
        || durable_binding != staged.binding
        || durable_dispatch != dispatch
    {
        return Err(StorageError::request_conflict(&request_id));
    }
    Ok(SnapshotProductCommit {
        receipt,
        candidate: durable_candidate,
        snapshot: durable_snapshot,
        binding: durable_binding,
        dispatch: durable_dispatch,
    })
}

/// Validates that the current durable Snapshot and runtime binding are the
/// exact pair supplied to execution/result ingress.
///
/// # Errors
/// Missing, foreign, stale, or tampered bindings fail closed.
pub fn validate_snapshot_binding(
    storage: &dyn ProductStateStorage,
    check: &SnapshotBindingCheck,
) -> Result<SnapshotBindingValidated, StorageError> {
    let snapshot_state = storage
        .load_state(&snapshot_stream_id(&check.snapshot_id))?
        .ok_or_else(|| StorageError::invalid_input("Snapshot binding has no product Snapshot"))?;
    let binding_state = storage
        .load_state(&binding_stream_id(
            &check.snapshot_id,
            &check.execution_job_id,
            check.attempt,
        ))?
        .ok_or_else(|| StorageError::invalid_input("Snapshot binding has no runtime binding"))?;
    let snapshot: CanonicalSnapshot = serde_json::from_slice(&snapshot_state.payload)
        .map_err(|error| StorageError::adapter(error.to_string()))?;
    let binding: SnapshotVerificationBinding = serde_json::from_slice(&binding_state.payload)
        .map_err(|error| StorageError::adapter(error.to_string()))?;
    validate_binding(&binding)?;
    validate_session_snapshot(&binding, &snapshot)?;
    if snapshot.snapshot_id() != &check.snapshot_id || !verify_snapshot_seal(&snapshot) {
        return Err(StorageError::invalid_input(
            "Snapshot product is not an immutable sealed record",
        ));
    }
    if binding.execution_job_id != check.execution_job_id
        || binding.work_run_id != check.work_run_id
        || binding.product_session_id != check.product_session_id
        || binding.attempt != check.attempt
    {
        return Err(StorageError::invalid_input(
            "Snapshot binding is missing, foreign, or stale",
        ));
    }
    Ok(SnapshotBindingValidated {
        verification_plan: binding.verification_plan,
        verification_session: binding.verification_session,
        snapshot_id: snapshot.snapshot_id().clone(),
        candidate_id: snapshot.candidate_id().clone(),
        execution_job_id: binding.execution_job_id,
        work_run_id: binding.work_run_id,
        attempt: binding.attempt,
    })
}

fn validate_candidate(candidate: &Candidate) -> Result<(), StorageError> {
    if candidate.schema_version != SchemaVersion::WinwincodeV1
        || candidate.attempt <= 0
        || candidate.contract_revision.0 < 0
        || !is_canonical_prefixed_id(&candidate.id.0, "cnd_")
        || !is_canonical_prefixed_id(&candidate.work_contract_id.0, "wct_")
        || !is_canonical_prefixed_id(&candidate.work_item_id.0, "wit_")
        || !is_canonical_prefixed_id(&candidate.work_run_id.0, "wrn_")
        || !is_canonical_prefixed_id(&candidate.producer_worker_session_id.0, "wsn_")
        || candidate.candidate_ref
            != format!("refs/winwincode/candidates/{}", candidate.candidate_commit)
        || !valid_digest(&candidate.candidate_digest.0)
    {
        return Err(StorageError::invalid_input("Snapshot Candidate is invalid"));
    }
    Ok(())
}

fn validate_session_snapshot(
    binding: &SnapshotVerificationBinding,
    snapshot: &CanonicalSnapshot,
) -> Result<(), StorageError> {
    if binding.verification_session.snapshot_id != *snapshot.snapshot_id()
        || binding.verification_session.candidate_id != *snapshot.candidate_id()
    {
        return Err(StorageError::invalid_input(
            "VerificationSession names another Snapshot",
        ));
    }
    Ok(())
}

fn validate_binding(binding: &SnapshotVerificationBinding) -> Result<(), StorageError> {
    let session = &binding.verification_session;
    let plan = &binding.verification_plan;
    if session.schema_version != SchemaVersion::WinwincodeV1
        || plan.schema_version != SchemaVersion::WinwincodeV1
        || !is_canonical_prefixed_id(&session.id.0, "vsn_")
        || !is_canonical_prefixed_id(&plan.id.0, "vpl_")
        || session.id != session.verification_session_id
        || session.verification_plan_id != plan.id
        || session.attempt != binding.attempt
        || session.work_run_id != binding.work_run_id
        || session.session_identity.work_run_id.as_ref() != Some(&binding.work_run_id)
        || session.session_identity.product_session_id != binding.product_session_id
        || Some(&session.session_identity.worker_session_id) != binding.worker_session_id.as_ref()
        || Some(&session.session_identity.codex_thread_id) != binding.codex_thread_id.as_ref()
        || plan.work_run_id != binding.work_run_id
        || plan.permission_profile != "candidate-read-only"
        || plan.required_roles != [binding.verification_role.clone()]
        || plan.plan_revision.0 < 1
        || !valid_digest(&plan.candidate_digest.0)
    {
        return Err(StorageError::invalid_input(
            "VerificationSession or plan differs from runtime binding",
        ));
    }
    if binding.attempt <= 0
        || !is_canonical_prefixed_id(&binding.session_binding_id, "sbn_")
        || !is_canonical_prefixed_id(&binding.work_run_id.0, "wrn_")
        || !is_canonical_prefixed_id(&binding.execution_job_id.0, "job_")
        || !is_canonical_prefixed_id(&binding.product_session_id.0, "psn_")
        || binding
            .worker_session_id
            .as_ref()
            .is_some_and(|id| !is_canonical_prefixed_id(&id.0, "wsn_"))
        || binding
            .codex_thread_id
            .as_ref()
            .is_some_and(|id| !is_canonical_prefixed_id(&id.0, "cdx_"))
        || !matches!(
            binding.verification_role.as_str(),
            "reviewer" | "verifier" | "adversarial-verifier"
        )
    {
        return Err(StorageError::invalid_input(
            "Snapshot verification binding is invalid",
        ));
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn load_durable_product<T: serde::de::DeserializeOwned>(
    storage: &dyn ProductStateStorage,
    stream_id: &str,
) -> Result<T, StorageError> {
    let state = storage
        .load_state(stream_id)?
        .ok_or_else(|| StorageError::adapter("Snapshot product replay is missing durable state"))?;
    serde_json::from_slice(&state.payload).map_err(|error| StorageError::adapter(error.to_string()))
}

fn dispatch_from_receipt(
    receipt: &CommitReceipt,
) -> Result<SnapshotVerificationDispatch, StorageError> {
    if receipt.events.len() != 1 {
        return Err(StorageError::adapter(
            "Snapshot product receipt must contain exactly one dispatch event",
        ));
    }
    let event = &receipt.events[0];
    let payload: SnapshotVerificationDispatchPayload = serde_json::from_slice(&event.payload)
        .map_err(|error| StorageError::adapter(error.to_string()))?;
    if payload.session_binding.trim().is_empty() {
        return Err(StorageError::adapter(
            "Snapshot product dispatch has an empty session binding",
        ));
    }
    Ok(SnapshotVerificationDispatch {
        event_id: event.event_id.clone(),
        topic: event.topic.clone(),
        snapshot_id: payload.snapshot,
        candidate_id: payload.candidate,
        work_run_id: payload.work_run,
        execution_job_id: payload.execution_job,
        payload: event.payload.clone(),
        attempt: payload.attempt,
    })
}

fn snapshot_stream_id(snapshot_id: &SnapshotId) -> String {
    format!("snapshot-product:v1:{}", snapshot_id.0)
}

fn candidate_stream_id(candidate_id: &CandidateId) -> String {
    format!("candidate-product:v1:{}", candidate_id.0)
}

fn binding_stream_id(
    snapshot_id: &SnapshotId,
    execution_job_id: &ExecutionJobId,
    attempt: i64,
) -> String {
    format!(
        "snapshot-binding:v1:{}:{}:{attempt}",
        snapshot_id.0, execution_job_id.0
    )
}

fn snapshot_dispatch_event_id(execution_job_id: &ExecutionJobId, attempt: i64) -> String {
    format!("snapshot-dispatch:{}:{attempt}", execution_job_id.0)
}
