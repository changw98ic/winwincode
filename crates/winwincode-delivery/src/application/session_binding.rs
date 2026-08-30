// SPDX-License-Identifier: Apache-2.0

//! Exact execution-session binding transitions.

use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, FencingToken, LeaseId,
    ProductSessionId, RequestId, Sha256Digest, StageRunId, WorkerId, WorkerInstanceId,
    WorkerSessionId,
};
use winwincode_storage::{
    ExecutionLeaseRecord, ExecutionQueueScope, ExecutionScopeReplacementAuthority,
    WorkerSlotAuthority,
};

use crate::domain::{Delivery, SessionBindingSourceProvenance, StageRunStatus};

use super::{CoordinationError, CoordinationErrorCode, require_mutation_time};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingIdentity {
    pub delivery_id: DeliveryId,
    pub delivery_task_id: Option<DeliveryTaskId>,
    pub stage_run_id: StageRunId,
    pub product_session_id: ProductSessionId,
    pub execution_job_id: ExecutionJobId,
}

/// Scheduler-owned authority that may complete one pending `SessionBinding`.
///
/// These values are copied into the canonical Delivery binding only after the
/// caller has proved that they belong to the exact active `StageRun`. Once
/// persisted, a different lease, Worker, instance, attempt, fence, or source
/// cannot replace them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingAuthority {
    worker_id: WorkerId,
    worker_instance_id: WorkerInstanceId,
    lease_id: LeaseId,
    attempt: u64,
    fencing_token: FencingToken,
    worker_session_id: WorkerSessionId,
    source_provenance: SessionBindingSourceProvenance,
}

/// Scheduler-sealed old-to-new execution authority accepted by one Delivery.
///
/// Only [`Self::from_scheduler`] can construct production values. The
/// Delivery transition revalidates the predecessor against its complete
/// `StageRun` binding before clearing it for the exact successor attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryExecutionAttemptReplacement {
    receipt_id: RequestId,
    receipt_digest: Sha256Digest,
    scope: ExecutionQueueScope,
    stage_run_id: Option<StageRunId>,
    predecessor_lease: ExecutionLeaseRecord,
    predecessor_worker_session_id: Option<WorkerSessionId>,
    predecessor_slot: Option<WorkerSlotAuthority>,
    successor_lease: ExecutionLeaseRecord,
}

impl DeliveryExecutionAttemptReplacement {
    /// Copies one already verified scheduler seal into the Delivery owner.
    #[must_use]
    pub fn from_scheduler(authority: &ExecutionScopeReplacementAuthority) -> Self {
        Self {
            receipt_id: authority.receipt_id().clone(),
            receipt_digest: authority.receipt_digest().clone(),
            scope: authority.scope().clone(),
            stage_run_id: authority.stage_run_id().cloned(),
            predecessor_lease: authority.predecessor_lease().clone(),
            predecessor_worker_session_id: authority.previous_worker_session_id().cloned(),
            predecessor_slot: authority.predecessor_slot().cloned(),
            successor_lease: authority.replacement_lease().clone(),
        }
    }

    #[must_use]
    pub(crate) const fn receipt_id(&self) -> &RequestId {
        &self.receipt_id
    }

    pub(crate) fn store_request_digest(&self) -> Result<String, CoordinationError> {
        self.receipt_digest
            .0
            .strip_prefix("sha256:")
            .filter(|digest| digest.len() == 64)
            .map(str::to_owned)
            .ok_or_else(|| replacement_conflict("replacement receipt digest is not canonical"))
    }
}

impl SessionBindingAuthority {
    /// Creates authority for a validated typed `ExecutionPort` message.
    ///
    /// The Control Plane must validate the message against its scheduler-owned
    /// lease before calling this constructor. Keeping the source provenance
    /// private prevents callers from changing an `ExecutionPort` authority into
    /// an unqualified or migration source.
    #[must_use]
    pub fn from_execution_port(
        worker_id: WorkerId,
        worker_instance_id: WorkerInstanceId,
        lease_id: LeaseId,
        attempt: u64,
        fencing_token: FencingToken,
        worker_session_id: WorkerSessionId,
        message_id: winwincode_domain::ExecutionMessageId,
    ) -> Self {
        Self {
            worker_id,
            worker_instance_id,
            lease_id,
            attempt,
            fencing_token,
            worker_session_id,
            source_provenance: SessionBindingSourceProvenance::from_execution_port(message_id),
        }
    }
}

/// Rotates one completely bound running Delivery attempt to the exact
/// scheduler-sealed successor and leaves its binding pending.
///
/// # Errors
///
/// Rejects a stale revision, an incomplete predecessor session, a foreign
/// scope or `StageRun`, or any mismatch between the persisted binding and the
/// sealed predecessor/successor lineage.
pub fn replace_execution_attempt_with_authority(
    delivery: &Delivery,
    expected_revision: u64,
    identity: &SessionBindingIdentity,
    replacement: &DeliveryExecutionAttemptReplacement,
    now_millis: u64,
) -> Result<Delivery, CoordinationError> {
    require_revision(delivery, expected_revision)?;
    require_mutation_time(delivery, now_millis)?;
    let index = exact_binding_index(delivery, identity)?;
    let run = active_run(delivery, identity)?;
    let binding = &delivery.snapshot().session_bindings[index];
    validate_replacement_identity(identity, run, binding, replacement)?;

    let mut snapshot = delivery.clone().into_snapshot();
    let run = snapshot
        .stage_runs
        .iter_mut()
        .find(|run| run.id == identity.stage_run_id)
        .ok_or_else(|| replacement_conflict("replacement StageRun disappeared"))?;
    run.attempt = replacement.successor_lease.attempt;
    let binding = &mut snapshot.session_bindings[index];
    binding.worker_session_id = None;
    binding.codex_thread_id = None;
    binding.worker_id = None;
    binding.worker_instance_id = None;
    binding.lease_id = None;
    binding.attempt = replacement.successor_lease.attempt;
    binding.fencing_token = None;
    binding.source_provenance = SessionBindingSourceProvenance::pending_delivery_advance();
    binding.bound_at_millis = now_millis;
    snapshot.revision += 1;
    snapshot.updated_at_millis = now_millis;
    Delivery::try_from_snapshot(snapshot).map_err(|error| replacement_conflict(error.to_string()))
}

fn validate_replacement_identity(
    identity: &SessionBindingIdentity,
    run: &crate::domain::StageRun,
    binding: &crate::domain::SessionBinding,
    replacement: &DeliveryExecutionAttemptReplacement,
) -> Result<(), CoordinationError> {
    let predecessor = &replacement.predecessor_lease;
    let successor = &replacement.successor_lease;
    if replacement.scope.delivery_id.as_ref() != Some(&identity.delivery_id)
        || replacement.scope.product_session_id != identity.product_session_id
        || replacement.stage_run_id.as_ref() != Some(&identity.stage_run_id)
        || predecessor.job_id != identity.execution_job_id
        || successor.job_id != identity.execution_job_id
        || predecessor.attempt != run.attempt
        || successor.attempt != predecessor.attempt.saturating_add(1)
        || predecessor.payload_digest != successor.payload_digest
        || predecessor.worker_id != successor.worker_id
        || predecessor.worker_instance_id == successor.worker_instance_id
        || predecessor.lease_id == successor.lease_id
        || predecessor.fencing_token == successor.fencing_token
    {
        return Err(replacement_conflict(
            "scheduler replacement does not match the active Delivery execution",
        ));
    }
    if let Some(slot) = replacement.predecessor_slot.as_ref() {
        let binding_matches = binding.attempt == predecessor.attempt
            && binding.worker_id.as_ref() == Some(&predecessor.worker_id)
            && binding.worker_instance_id.as_ref() == Some(&predecessor.worker_instance_id)
            && binding.lease_id.as_ref() == Some(&predecessor.lease_id)
            && binding.fencing_token.as_ref() == Some(&predecessor.fencing_token)
            && binding.worker_session_id.as_ref()
                == replacement.predecessor_worker_session_id.as_ref()
            && binding.worker_session_id.as_ref() == Some(&slot.worker_session_id)
            && binding.codex_thread_id.as_ref() == Some(&slot.codex_thread_id)
            && slot.job_id == predecessor.job_id
            && slot.lease_id == predecessor.lease_id
            && slot.worker_id == predecessor.worker_id
            && slot.worker_instance_id == predecessor.worker_instance_id
            && slot.attempt == predecessor.attempt
            && slot.fencing_token == predecessor.fencing_token;
        if !binding_matches {
            return Err(replacement_conflict(
                "scheduler replacement predecessor is not the complete Delivery binding",
            ));
        }
    } else {
        // A Worker may have accepted a dispatch before it opened its
        // WorkerSession slot. In that crash window the Delivery binding
        // remains the original pending placeholder; rotate only its
        // attempt and leave all runtime owners empty for the successor.
        let pending = binding.attempt == predecessor.attempt
            && binding.worker_session_id.is_none()
            && binding.codex_thread_id.is_none()
            && binding.worker_id.is_none()
            && binding.worker_instance_id.is_none()
            && binding.lease_id.is_none()
            && binding.fencing_token.is_none();
        if !pending {
            return Err(replacement_conflict(
                "slotless scheduler replacement requires a pending Delivery binding",
            ));
        }
    }
    Ok(())
}

fn replacement_conflict(message: impl Into<String>) -> CoordinationError {
    CoordinationError::new(CoordinationErrorCode::BindingConflict, message)
}

/// Records a `WorkerSession` together with the complete scheduler lease
/// authority that produced it.
///
/// The mutation is idempotent only for the same complete authority. A
/// replacement lease or Worker identity is rejected before a Delivery copy is
/// changed.
///
/// # Errors
///
/// Fails closed on stale revision, inactive run, mismatched attempt, changed
/// authority, or a `WorkerSession` already owned by another binding.
pub fn accept_worker_session_with_authority(
    delivery: &Delivery,
    expected_revision: u64,
    identity: &SessionBindingIdentity,
    authority: &SessionBindingAuthority,
    now_millis: u64,
) -> Result<Delivery, CoordinationError> {
    require_revision(delivery, expected_revision)?;
    require_mutation_time(delivery, now_millis)?;
    let index = exact_binding_index(delivery, identity)?;
    let run = active_run(delivery, identity)?;
    if run.attempt != authority.attempt {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "Worker authority attempt does not match the exact active StageRun",
        ));
    }
    let current = &delivery.snapshot().session_bindings[index];
    validate_authority_transition(current, authority)?;
    if delivery
        .snapshot()
        .session_bindings
        .iter()
        .enumerate()
        .any(|(other_index, binding)| {
            other_index != index
                && binding.worker_session_id.as_ref() == Some(&authority.worker_session_id)
        })
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "WorkerSession is already assigned to another StageRun",
        ));
    }
    if current.worker_session_id.is_some()
        && current.worker_session_id.as_ref() != Some(&authority.worker_session_id)
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "SessionBinding already has another WorkerSession",
        ));
    }
    if current.worker_session_id.as_ref() == Some(&authority.worker_session_id)
        && current.worker_id.is_some()
        && current.source_provenance == authority.source_provenance
    {
        return Ok(delivery.clone());
    }
    let mut snapshot = delivery.clone().into_snapshot();
    let binding = &mut snapshot.session_bindings[index];
    binding.worker_session_id = Some(authority.worker_session_id.clone());
    binding.worker_id = Some(authority.worker_id.clone());
    binding.worker_instance_id = Some(authority.worker_instance_id.clone());
    binding.lease_id = Some(authority.lease_id.clone());
    binding.attempt = authority.attempt;
    binding.fencing_token = Some(authority.fencing_token.clone());
    binding.source_provenance = authority.source_provenance.clone();
    snapshot.revision += 1;
    snapshot.updated_at_millis = now_millis;
    Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::BindingConflict, error.to_string())
    })
}

/// Records a `CodexThread` while revalidating the complete Worker lease
/// authority that already owns the binding.
///
/// # Errors
///
/// Fails closed on stale revision, inactive run, changed authority, a missing
/// accepted `WorkerSession`, or a `CodexThread` owned by another binding.
pub fn report_codex_thread_with_authority(
    delivery: &Delivery,
    expected_revision: u64,
    identity: &SessionBindingIdentity,
    authority: &SessionBindingAuthority,
    codex_thread_id: CodexThreadId,
    now_millis: u64,
) -> Result<Delivery, CoordinationError> {
    require_revision(delivery, expected_revision)?;
    require_mutation_time(delivery, now_millis)?;
    let index = exact_binding_index(delivery, identity)?;
    let run = active_run(delivery, identity)?;
    if run.attempt != authority.attempt {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "CodexThread authority attempt does not match the exact active StageRun",
        ));
    }
    let current = &delivery.snapshot().session_bindings[index];
    if current.worker_session_id.as_ref() != Some(&authority.worker_session_id) {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "CodexThread report does not match the accepted WorkerSession",
        ));
    }
    validate_authority_transition(current, authority)?;
    if delivery
        .snapshot()
        .session_bindings
        .iter()
        .enumerate()
        .any(|(other_index, binding)| {
            other_index != index && binding.codex_thread_id.as_ref() == Some(&codex_thread_id)
        })
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "CodexThread is already assigned to another StageRun",
        ));
    }
    if current.codex_thread_id.as_ref() == Some(&codex_thread_id)
        && current.worker_id.is_some()
        && current.source_provenance == authority.source_provenance
    {
        return Ok(delivery.clone());
    }
    if current.codex_thread_id.is_some()
        && current.codex_thread_id.as_ref() != Some(&codex_thread_id)
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "SessionBinding already has another CodexThread",
        ));
    }
    let mut snapshot = delivery.clone().into_snapshot();
    let binding = &mut snapshot.session_bindings[index];
    binding.worker_id = Some(authority.worker_id.clone());
    binding.worker_instance_id = Some(authority.worker_instance_id.clone());
    binding.lease_id = Some(authority.lease_id.clone());
    binding.attempt = authority.attempt;
    binding.fencing_token = Some(authority.fencing_token.clone());
    binding.source_provenance = authority.source_provenance.clone();
    binding.codex_thread_id = Some(codex_thread_id);
    snapshot.revision += 1;
    snapshot.updated_at_millis = now_millis;
    Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::BindingConflict, error.to_string())
    })
}

fn require_revision(delivery: &Delivery, expected_revision: u64) -> Result<(), CoordinationError> {
    if delivery.revision() == expected_revision {
        Ok(())
    } else {
        Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before SessionBinding update",
        ))
    }
}

fn exact_binding_index(
    delivery: &Delivery,
    identity: &SessionBindingIdentity,
) -> Result<usize, CoordinationError> {
    let mut matches = delivery
        .snapshot()
        .session_bindings
        .iter()
        .enumerate()
        .filter(|(_, binding)| {
            binding.delivery_id == identity.delivery_id
                && binding.delivery_task_id == identity.delivery_task_id
                && binding.stage_run_id == identity.stage_run_id
                && binding.product_session_id == identity.product_session_id
                && binding.execution_job_id == identity.execution_job_id
        });
    let (index, _) = matches.next().ok_or_else(|| {
        CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "no SessionBinding matches the exact Delivery stage and job identity",
        )
    })?;
    if matches.next().is_some() {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "more than one SessionBinding matches the stage and job identity",
        ));
    }
    Ok(index)
}

fn validate_authority_transition(
    current: &crate::domain::SessionBinding,
    authority: &SessionBindingAuthority,
) -> Result<(), CoordinationError> {
    let current_authority_count = usize::from(current.worker_id.is_some())
        + usize::from(current.worker_instance_id.is_some())
        + usize::from(current.lease_id.is_some())
        + usize::from(current.fencing_token.is_some());
    if current_authority_count != 0 && current_authority_count != 4 {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "SessionBinding contains a partial persisted lease authority",
        ));
    }
    let matches = current.worker_id.as_ref() == Some(&authority.worker_id)
        && current.worker_instance_id.as_ref() == Some(&authority.worker_instance_id)
        && current.lease_id.as_ref() == Some(&authority.lease_id)
        && current.fencing_token.as_ref() == Some(&authority.fencing_token)
        && current.attempt == authority.attempt
        && (current.worker_id.is_none()
            || current.source_provenance == authority.source_provenance);
    if current.worker_id.is_some() && !matches {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "Worker authority would replace the current lease, Worker, instance, attempt, fence, or source",
        ));
    }
    Ok(())
}

fn active_run<'delivery>(
    delivery: &'delivery Delivery,
    identity: &SessionBindingIdentity,
) -> Result<&'delivery crate::domain::StageRun, CoordinationError> {
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| run.id == identity.stage_run_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::BindingConflict,
                "SessionBinding StageRun does not exist",
            )
        })?;
    if run.delivery_id != identity.delivery_id
        || run.delivery_task_id != identity.delivery_task_id
        || !matches!(
            run.status,
            StageRunStatus::Running | StageRunStatus::Waiting
        )
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "SessionBinding does not match an active exact StageRun",
        ));
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::{
        DeliveryExecutionAttemptReplacement, SessionBindingAuthority, SessionBindingIdentity,
        accept_worker_session_with_authority, replace_execution_attempt_with_authority,
        report_codex_thread_with_authority,
    };
    use crate::domain::SessionBindingSourceProvenance;
    use crate::domain::{Delivery, DeliveryStatus, StageRunStatus, test_fixture};
    use winwincode_domain::{
        CodexThreadId, ExecutionMessageId, FencingToken, Instant, LeaseId, RequestId, Sha256Digest,
        WorkerId, WorkerInstanceId, WorkerSessionId,
    };
    use winwincode_storage::{ExecutionLeaseRecord, WorkerSlotAuthority};

    fn active_delivery() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Verifying;
        let run = &mut snapshot.stage_runs[0];
        run.status = StageRunStatus::Running;
        run.finished_at_millis = None;
        let binding = &mut snapshot.session_bindings[0];
        binding.worker_session_id = None;
        binding.codex_thread_id = None;
        binding.worker_id = None;
        binding.worker_instance_id = None;
        binding.lease_id = None;
        binding.fencing_token = None;
        binding.attempt = run.attempt;
        binding.source_provenance =
            SessionBindingSourceProvenance::delivery_advance("delivery.advance");
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.updated_at_millis = 1_800_000_000_100;
        Delivery::try_from_snapshot(snapshot).expect("active Delivery")
    }

    fn identity(delivery: &Delivery) -> SessionBindingIdentity {
        let binding = &delivery.snapshot().session_bindings[0];
        SessionBindingIdentity {
            delivery_id: binding.delivery_id.clone(),
            delivery_task_id: binding.delivery_task_id.clone(),
            stage_run_id: binding.stage_run_id.clone(),
            product_session_id: binding.product_session_id.clone(),
            execution_job_id: binding.execution_job_id.clone(),
        }
    }

    fn authority(worker_session_id: &str) -> SessionBindingAuthority {
        SessionBindingAuthority {
            worker_id: WorkerId("wrk_01J00000000000000000000000".into()),
            worker_instance_id: WorkerInstanceId("wki_01J00000000000000000000000".into()),
            lease_id: LeaseId("lse_01J00000000000000000000000".into()),
            attempt: 1,
            fencing_token: FencingToken("7".into()),
            worker_session_id: WorkerSessionId(worker_session_id.into()),
            source_provenance: SessionBindingSourceProvenance::execution_port(ExecutionMessageId(
                "msg_01J00000000000000000000000".into(),
            )),
        }
    }

    fn running_bound_delivery() -> Delivery {
        let pending = active_delivery();
        let identity = identity(&pending);
        let authority = authority("wsn_01J00000000000000000000000");
        let worker_bound = accept_worker_session_with_authority(
            &pending,
            pending.revision(),
            &identity,
            &authority,
            1_800_000_000_110,
        )
        .expect("worker binding");
        report_codex_thread_with_authority(
            &worker_bound,
            worker_bound.revision(),
            &identity,
            &authority,
            CodexThreadId("cdx_01J00000000000000000000000".into()),
            1_800_000_000_111,
        )
        .expect("Codex thread binding")
    }

    fn replacement(delivery: &Delivery) -> DeliveryExecutionAttemptReplacement {
        let binding = &delivery.snapshot().session_bindings[0];
        let predecessor_lease = ExecutionLeaseRecord {
            job_id: binding.execution_job_id.clone(),
            lease_id: binding.lease_id.clone().expect("old lease"),
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            worker_id: binding.worker_id.clone().expect("old Worker"),
            worker_instance_id: binding.worker_instance_id.clone().expect("old instance"),
            attempt: binding.attempt,
            fencing_token: binding.fencing_token.clone().expect("old fence"),
            issued_at: Instant("2027-10-01T10:00:01.000Z".into()),
            expires_at: Instant("2027-10-01T10:00:20.000Z".into()),
        };
        DeliveryExecutionAttemptReplacement {
            receipt_id: RequestId("req_01J00000000000000000000009".into()),
            receipt_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            scope: winwincode_storage::ExecutionQueueScope {
                organization_id: winwincode_domain::OrganizationId(
                    "org_01J00000000000000000000000".into(),
                ),
                workspace_id: winwincode_domain::WorkspaceId(
                    "wsp_01J00000000000000000000000".into(),
                ),
                project_id: winwincode_domain::ProjectId("prj_01J00000000000000000000000".into()),
                repository_id: winwincode_domain::RepositoryId(
                    "rep_01J00000000000000000000000".into(),
                ),
                product_session_id: binding.product_session_id.clone(),
                delivery_id: Some(binding.delivery_id.clone()),
            },
            stage_run_id: Some(delivery.snapshot().stage_runs[0].id.clone()),
            predecessor_worker_session_id: binding.worker_session_id.clone(),
            predecessor_slot: Some(WorkerSlotAuthority {
                worker_id: predecessor_lease.worker_id.clone(),
                worker_instance_id: predecessor_lease.worker_instance_id.clone(),
                worker_session_id: binding.worker_session_id.clone().expect("old session"),
                codex_thread_id: binding.codex_thread_id.clone().expect("old thread"),
                job_id: predecessor_lease.job_id.clone(),
                lease_id: predecessor_lease.lease_id.clone(),
                attempt: predecessor_lease.attempt,
                fencing_token: predecessor_lease.fencing_token.clone(),
            }),
            successor_lease: ExecutionLeaseRecord {
                job_id: predecessor_lease.job_id.clone(),
                lease_id: LeaseId("lse_01J00000000000000000000009".into()),
                payload_digest: predecessor_lease.payload_digest.clone(),
                worker_id: predecessor_lease.worker_id.clone(),
                worker_instance_id: WorkerInstanceId("wki_01J00000000000000000000009".into()),
                attempt: predecessor_lease.attempt + 1,
                fencing_token: FencingToken("8".into()),
                issued_at: Instant("2027-10-01T10:00:21.000Z".into()),
                expires_at: Instant("2027-10-01T10:00:40.000Z".into()),
            },
            predecessor_lease,
        }
    }

    #[test]
    fn sealed_running_attempt_replacement_clears_only_the_old_execution_binding() {
        let delivery = running_bound_delivery();
        let replacement = replacement(&delivery);

        let replaced = replace_execution_attempt_with_authority(
            &delivery,
            delivery.revision(),
            &identity(&delivery),
            &replacement,
            1_800_000_000_121,
        )
        .expect("sealed replacement");

        let run = &replaced.snapshot().stage_runs[0];
        let binding = &replaced.snapshot().session_bindings[0];
        assert_eq!(run.attempt, 2);
        assert_eq!(binding.attempt, 2);
        assert!(binding.worker_session_id.is_none());
        assert!(binding.codex_thread_id.is_none());
        assert!(binding.worker_id.is_none());
        assert!(binding.worker_instance_id.is_none());
        assert!(binding.lease_id.is_none());
        assert!(binding.fencing_token.is_none());
        assert_eq!(
            binding.source_provenance,
            SessionBindingSourceProvenance::delivery_advance("delivery.advance")
        );
        assert_eq!(replaced.revision(), delivery.revision() + 1);

        let mut changed = replacement;
        changed.predecessor_lease.fencing_token = FencingToken("9".into());
        assert!(
            replace_execution_attempt_with_authority(
                &delivery,
                delivery.revision(),
                &identity(&delivery),
                &changed,
                1_800_000_000_121,
            )
            .is_err()
        );
    }

    #[test]
    fn replaced_worker_authority_is_rejected_without_a_delivery_write() {
        let delivery = active_delivery();
        let binding_identity = identity(&delivery);
        let first = authority("wsn_01J00000000000000000000000");
        let accepted = accept_worker_session_with_authority(
            &delivery,
            delivery.revision(),
            &binding_identity,
            &first,
            1_800_000_000_110,
        )
        .expect("first fenced authority");
        let before = accepted.encode_json().expect("accepted Delivery");

        let mut replacement = first.clone();
        replacement.lease_id = LeaseId("lse_01J00000000000000000000001".into());
        replacement.fencing_token = FencingToken("8".into());
        let error = accept_worker_session_with_authority(
            &accepted,
            accepted.revision(),
            &binding_identity,
            &replacement,
            1_800_000_000_111,
        )
        .expect_err("replacement authority must be rejected");

        assert_eq!(error.code(), super::CoordinationErrorCode::BindingConflict);
        assert_eq!(
            accepted.encode_json().expect("unchanged Delivery"),
            before,
            "rejected replacement must not mutate the Delivery"
        );
    }
}
