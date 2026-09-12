// SPDX-License-Identifier: Apache-2.0

//! Exact execution-session binding transitions.

use winwincode_domain::{
    CodexThreadId, DeliveryId, ExecutionJobId, FencingToken, LeaseId, ProductSessionId, RequestId,
    Revision, Sha256Digest, WorkContractId, WorkItemId, WorkItemState, WorkRunId, WorkerId,
    WorkerInstanceId, WorkerSessionId,
};
use winwincode_storage::{
    ExecutionLeaseRecord, ExecutionQueueScope, ExecutionScopeReplacementAuthority,
    WorkerSlotAuthority,
};

use crate::domain::{
    Delivery, SessionBindingSourceKind, SessionBindingSourceProvenance, SessionRuntimeContext,
};

use super::{CoordinationError, CoordinationErrorCode, require_mutation_time};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBindingIdentity {
    pub delivery_id: DeliveryId,
    pub work_contract_id: WorkContractId,
    pub work_contract_revision: Revision,
    pub work_item_id: WorkItemId,
    pub work_item_revision: Revision,
    pub work_run_id: WorkRunId,
    pub product_session_id: ProductSessionId,
    pub execution_job_id: ExecutionJobId,
}

/// Scheduler-owned authority that may complete one pending `SessionBinding`.
///
/// These values are copied into the canonical Delivery binding only after the
/// caller has proved that they belong to the exact active `WorkRun`. Once
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
/// `WorkRun` binding before clearing it for the exact successor attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryExecutionAttemptReplacement {
    receipt_id: RequestId,
    receipt_digest: Sha256Digest,
    scope: ExecutionQueueScope,
    work_run_id: Option<WorkRunId>,
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
            work_run_id: authority.work_run_id().cloned(),
            predecessor_lease: authority.predecessor_lease().clone(),
            predecessor_worker_session_id: authority.previous_worker_session_id().cloned(),
            predecessor_slot: authority.predecessor_slot().cloned(),
            successor_lease: authority.replacement_lease().clone(),
        }
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

/// Completes a replacement after the successor `WorkerSession` has been
/// accepted. The predecessor remains as a terminal audit run; the successor
/// is appended with the scheduler-sealed `WorkRun` and the newly accepted
/// session authority.
///
/// # Errors
/// Returns a conflict when the revision, predecessor, successor, lease, or
/// worker authority does not match the scheduler-sealed replacement.
pub fn accept_replacement_worker_session_with_authority(
    delivery: &Delivery,
    expected_revision: u64,
    identity: &SessionBindingIdentity,
    replacement: &DeliveryExecutionAttemptReplacement,
    authority: &SessionBindingAuthority,
    now_millis: u64,
) -> Result<Delivery, CoordinationError> {
    require_revision(delivery, expected_revision)?;
    require_mutation_time(delivery, now_millis)?;
    let (index, successor_id) =
        validated_replacement_successor(delivery, identity, replacement, authority)?;
    let mut snapshot = delivery.clone().into_snapshot();
    let predecessor_run = snapshot
        .work_run_aggregate
        .runs
        .iter_mut()
        .find(|run| run.id == identity.work_run_id)
        .ok_or_else(|| replacement_conflict("predecessor WorkRun disappeared"))?;
    predecessor_run.state = winwincode_domain::WorkRunState::Failed;
    predecessor_run.revision = predecessor_run
        .revision
        .0
        .checked_add(1)
        .map(Revision)
        .ok_or_else(|| replacement_conflict("predecessor WorkRun revision overflow"))?;
    let mut successor = predecessor_run.clone();
    successor.id = successor_id.clone();
    successor.revision = Revision(1);
    successor.attempt = i64::try_from(replacement.successor_lease.attempt)
        .map_err(|_| replacement_conflict("successor attempt is out of range"))?;
    successor.lease_id = replacement.successor_lease.lease_id.clone();
    successor
        .fencing_token
        .clone_from(&replacement.successor_lease.fencing_token.0);
    successor.worker_id = authority.worker_id.clone();
    successor.worker_instance_id = authority.worker_instance_id.clone();
    successor.worker_session_id = authority.worker_session_id.clone();
    successor.codex_thread_id = None;
    successor.state = winwincode_domain::WorkRunState::Leased;
    successor.candidate_digest = None;
    let item_index = snapshot
        .work_run_aggregate
        .items
        .iter()
        .position(|item| item.id == successor.work_item_id)
        .ok_or_else(|| replacement_conflict("successor WorkItem disappeared"))?;
    let read_only = matches!(
        snapshot.session_bindings[index]
            .execution_profile
            .as_deref(),
        Some("reviewer" | "verifier" | "adversarial-verifier")
    );
    let expected_state = if read_only {
        WorkItemState::CandidateReady
    } else {
        WorkItemState::InProgress
    };
    if snapshot.work_run_aggregate.items[item_index].state != expected_state
        || snapshot.work_run_aggregate.items[item_index].revision != successor.work_item_revision
    {
        return Err(replacement_conflict(
            "replacement requires the exact in-progress WorkItem revision from the accepted job",
        ));
    }
    // The scheduler seal commits the intermediate Ready state and successor together.
    let item_revision = snapshot.work_run_aggregate.items[item_index]
        .revision
        .clone();
    successor.work_item_revision = item_revision.clone();
    if read_only {
        snapshot
            .work_run_aggregate
            .start_verification(&successor.work_item_id)
            .map_err(|error| {
                replacement_conflict(format!("verification replacement rejected: {error:?}"))
            })?;
        // The exact same-job successor attempt was checked against the scheduler seal above.
        snapshot.work_run_aggregate.runs.push(successor);
        snapshot.work_run_aggregate.validate().map_err(|error| {
            replacement_conflict(format!("verification successor rejected: {error:?}"))
        })?;
    } else {
        snapshot
            .work_run_aggregate
            .transition_item_state(&successor.work_item_id, WorkItemState::Ready)
            .map_err(|error| replacement_conflict(format!("retry state rejected: {error:?}")))?;
        snapshot
            .work_run_aggregate
            .append_run(successor)
            .map_err(|error| {
                replacement_conflict(format!("successor WorkRun rejected: {error:?}"))
            })?;
    }
    snapshot.session_bindings.push(successor_binding(
        &snapshot.session_bindings[index],
        &successor_id,
        item_revision,
        authority,
        replacement.successor_lease.attempt,
        now_millis,
    ));
    snapshot.revision = snapshot
        .revision
        .checked_add(1)
        .ok_or_else(|| replacement_conflict("Delivery revision overflow"))?;
    snapshot.updated_at_millis = now_millis;
    Delivery::try_from_snapshot(snapshot).map_err(|error| replacement_conflict(error.to_string()))
}

fn validated_replacement_successor(
    delivery: &Delivery,
    identity: &SessionBindingIdentity,
    replacement: &DeliveryExecutionAttemptReplacement,
    authority: &SessionBindingAuthority,
) -> Result<(usize, WorkRunId), CoordinationError> {
    let index = exact_binding_index(delivery, identity)?;
    let predecessor = active_run(delivery, identity)?;
    let successor_id = replacement
        .work_run_id
        .as_ref()
        .ok_or_else(|| replacement_conflict("replacement has no successor WorkRun"))?;
    validate_replacement_identity(
        identity,
        predecessor,
        &delivery.snapshot().session_bindings[index],
        replacement,
    )?;
    let successor = &replacement.successor_lease;
    if successor_id == &identity.work_run_id
        || authority.lease_id != successor.lease_id
        || authority.worker_id != successor.worker_id
        || authority.worker_instance_id != successor.worker_instance_id
        || authority.attempt != successor.attempt
        || authority.fencing_token != successor.fencing_token
    {
        return Err(replacement_conflict(
            "successor WorkerSession authority does not match replacement seal",
        ));
    }
    Ok((index, successor_id.clone()))
}

fn successor_binding(
    predecessor: &crate::domain::SessionBinding,
    successor_id: &WorkRunId,
    item_revision: Revision,
    authority: &SessionBindingAuthority,
    attempt: u64,
    bound_at_millis: u64,
) -> crate::domain::SessionBinding {
    let mut binding = predecessor.clone();
    binding.id = crate::domain::SessionBindingId(format!("{}:replacement:{attempt}", binding.id.0));
    binding.work_run_id = successor_id.clone();
    binding.work_item_revision = item_revision;
    binding.worker_session_id = Some(authority.worker_session_id.clone());
    binding.runtime_context = None;
    binding.codex_thread_id = None;
    binding.worker_id = Some(authority.worker_id.clone());
    binding.worker_instance_id = Some(authority.worker_instance_id.clone());
    binding.lease_id = Some(authority.lease_id.clone());
    binding.attempt = authority.attempt;
    binding.fencing_token = Some(authority.fencing_token.clone());
    binding.source_provenance = authority.source_provenance.clone();
    binding.bound_at_millis = bound_at_millis;
    binding
}

fn validate_replacement_identity(
    identity: &SessionBindingIdentity,
    run: &winwincode_domain::WorkRun,
    binding: &crate::domain::SessionBinding,
    replacement: &DeliveryExecutionAttemptReplacement,
) -> Result<(), CoordinationError> {
    let predecessor = &replacement.predecessor_lease;
    let successor = &replacement.successor_lease;
    if replacement.scope.delivery_id.as_ref() != Some(&identity.delivery_id)
        || replacement.scope.product_session_id != identity.product_session_id
        || predecessor.job_id != identity.execution_job_id
        || successor.job_id != identity.execution_job_id
        || run.attempt != i64::try_from(predecessor.attempt).unwrap_or(-1)
        || run.execution_job_id != predecessor.job_id
        || run.lease_id != predecessor.lease_id
        || run.fencing_token != predecessor.fencing_token.0
        || run.worker_id != predecessor.worker_id
        || run.worker_instance_id != predecessor.worker_instance_id
        || replacement
            .predecessor_worker_session_id
            .as_ref()
            .is_some_and(|session| session != &run.worker_session_id)
        || replacement.predecessor_slot.as_ref().is_some_and(|slot| {
            slot.worker_session_id != run.worker_session_id
                || Some(&slot.codex_thread_id) != run.codex_thread_id.as_ref()
        })
        || successor.attempt != predecessor.attempt.saturating_add(1)
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
            && binding.worker_session_id.as_ref()
                == replacement.predecessor_worker_session_id.as_ref()
            && binding.codex_thread_id.is_none()
            && binding.worker_id.as_ref() == Some(&predecessor.worker_id)
            && binding.worker_instance_id.as_ref() == Some(&predecessor.worker_instance_id)
            && binding.lease_id.as_ref() == Some(&predecessor.lease_id)
            && binding.fencing_token.as_ref() == Some(&predecessor.fencing_token);
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
    validate_run_authority(run, identity, authority, "Worker")?;
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
            "WorkerSession is already assigned to another WorkRun",
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
    binding.runtime_context = None;
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
    runtime_context: SessionRuntimeContext,
    now_millis: u64,
) -> Result<Delivery, CoordinationError> {
    require_revision(delivery, expected_revision)?;
    require_mutation_time(delivery, now_millis)?;
    let index = exact_binding_index(delivery, identity)?;
    let run = active_run(delivery, identity)?;
    validate_run_authority(run, identity, authority, "CodexThread")?;
    let current = &delivery.snapshot().session_bindings[index];
    if run.codex_thread_id != current.codex_thread_id {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "persisted WorkRun and SessionBinding disagree on the CodexThread",
        ));
    }
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
            "CodexThread is already assigned to another WorkRun",
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
    binding.codex_thread_id = Some(codex_thread_id.clone());
    binding.runtime_context = Some(runtime_context);
    let stored_run = snapshot
        .work_run_aggregate
        .runs
        .iter_mut()
        .find(|run| run.id == identity.work_run_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::BindingConflict,
                "WorkRun disappeared during CodexThread binding",
            )
        })?;
    stored_run.codex_thread_id = Some(codex_thread_id);
    stored_run.state = winwincode_domain::WorkRunState::Running;
    stored_run.revision.0 = stored_run
        .revision
        .0
        .checked_add(1)
        .filter(|revision| *revision <= super::super::domain::MAX_SAFE_INTEGER.cast_signed())
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::RevisionConflict,
                "WorkRun revision is exhausted",
            )
        })?;
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
                && binding.work_contract_id == identity.work_contract_id
                && binding.work_contract_revision == identity.work_contract_revision
                && binding.work_item_id == identity.work_item_id
                && binding.work_item_revision == identity.work_item_revision
                && WorkRunId(binding.work_run_id.0.clone()) == identity.work_run_id
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

fn validate_run_authority(
    run: &winwincode_domain::WorkRun,
    identity: &SessionBindingIdentity,
    authority: &SessionBindingAuthority,
    subject: &str,
) -> Result<(), CoordinationError> {
    let attempt = i64::try_from(authority.attempt).unwrap_or(-1);
    let run_fence = FencingToken(run.fencing_token.clone());
    let matches = run.id == identity.work_run_id
        && run.work_contract_id == identity.work_contract_id
        && run.contract_revision == identity.work_contract_revision
        && run.work_item_id == identity.work_item_id
        && run.work_item_revision == identity.work_item_revision
        && run.product_session_id.as_ref() == Some(&identity.product_session_id)
        && run.execution_job_id == identity.execution_job_id
        && run.attempt == attempt
        && run.lease_id == authority.lease_id
        && run.worker_id == authority.worker_id
        && run.worker_instance_id == authority.worker_instance_id
        && run.worker_session_id == authority.worker_session_id
        && run_fence == authority.fencing_token;
    if !matches {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            format!("{subject} authority does not match the complete active WorkRun"),
        ));
    }
    Ok(())
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
    let source_handoff = current.source_provenance.kind()
        == SessionBindingSourceKind::WorkRunDispatch
        && authority.source_provenance.kind() == SessionBindingSourceKind::ExecutionPort;
    let matches = current.worker_id.as_ref() == Some(&authority.worker_id)
        && current.worker_instance_id.as_ref() == Some(&authority.worker_instance_id)
        && current.lease_id.as_ref() == Some(&authority.lease_id)
        && current.fencing_token.as_ref() == Some(&authority.fencing_token)
        && current.attempt == authority.attempt
        && (current.worker_id.is_none()
            || current.source_provenance == authority.source_provenance
            || source_handoff);
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
) -> Result<&'delivery winwincode_domain::WorkRun, CoordinationError> {
    let run = delivery
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.id == identity.work_run_id)
        .ok_or_else(|| {
            CoordinationError::new(
                CoordinationErrorCode::BindingConflict,
                "SessionBinding WorkRun does not exist",
            )
        })?;
    if !matches!(
        run.state,
        winwincode_domain::WorkRunState::Leased | winwincode_domain::WorkRunState::Running
    ) || run.work_contract_id != identity.work_contract_id
        || run.contract_revision != identity.work_contract_revision
        || run.work_item_id != identity.work_item_id
        || run.work_item_revision != identity.work_item_revision
        || run.product_session_id.as_ref() != Some(&identity.product_session_id)
        || run.execution_job_id != identity.execution_job_id
    {
        return Err(CoordinationError::new(
            CoordinationErrorCode::BindingConflict,
            "SessionBinding does not match an active exact WorkRun",
        ));
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::{
        DeliveryExecutionAttemptReplacement, SessionBindingAuthority, SessionBindingIdentity,
        accept_replacement_worker_session_with_authority, accept_worker_session_with_authority,
        report_codex_thread_with_authority,
    };
    use crate::domain::SessionBindingSourceProvenance;
    use crate::domain::{Delivery, DeliveryStatus, test_fixture};
    use winwincode_domain::{
        CodexThreadId, ExecutionMessageId, FencingToken, Instant, LeaseId, RequestId, Sha256Digest,
        WorkerId, WorkerInstanceId, WorkerSessionId,
    };
    use winwincode_storage::{ExecutionLeaseRecord, WorkerSlotAuthority};

    fn active_delivery() -> Delivery {
        let mut snapshot = test_fixture();
        snapshot.status = DeliveryStatus::Ready;
        let accepted = authority("wsn_01J00000000000000000000000");
        let run = &mut snapshot.work_run_aggregate.runs[0];
        run.state = winwincode_domain::WorkRunState::Leased;
        run.codex_thread_id = None;
        run.worker_id = accepted.worker_id.clone();
        run.worker_instance_id = accepted.worker_instance_id.clone();
        run.worker_session_id = accepted.worker_session_id.clone();
        run.lease_id = accepted.lease_id.clone();
        run.fencing_token = accepted.fencing_token.0.clone();
        run.attempt = 1;
        snapshot.work_run_aggregate.items[0].state = winwincode_domain::WorkItemState::InProgress;
        let binding = &mut snapshot.session_bindings[0];
        binding.worker_session_id = Some(accepted.worker_session_id);
        binding.codex_thread_id = None;
        binding.runtime_context = None;
        binding.worker_id = Some(accepted.worker_id);
        binding.worker_instance_id = Some(accepted.worker_instance_id);
        binding.lease_id = Some(accepted.lease_id);
        binding.fencing_token = Some(accepted.fencing_token);
        binding.attempt = 1;
        binding.execution_profile = Some("executor".into());
        binding.source_provenance = SessionBindingSourceProvenance::pending_work_run_dispatch();
        snapshot.evidence.clear();
        snapshot.verdict = None;
        snapshot.updated_at_millis = 1_800_000_000_100;
        Delivery::try_from_snapshot(snapshot).expect("active Delivery")
    }

    fn identity(delivery: &Delivery) -> SessionBindingIdentity {
        let binding = &delivery.snapshot().session_bindings[0];
        SessionBindingIdentity {
            delivery_id: binding.delivery_id.clone(),
            work_contract_id: binding.work_contract_id.clone(),
            work_contract_revision: binding.work_contract_revision.clone(),
            work_item_id: binding.work_item_id.clone(),
            work_item_revision: binding.work_item_revision.clone(),
            work_run_id: binding.work_run_id.clone(),
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
        let mut runtime_context = test_fixture().session_bindings[0]
            .runtime_context
            .clone()
            .expect("fixture runtime context");
        runtime_context.agent_identity.worker_id = authority.worker_id.clone();
        runtime_context.agent_identity.role = "executor".into();
        report_codex_thread_with_authority(
            &worker_bound,
            worker_bound.revision(),
            &identity,
            &authority,
            CodexThreadId("cdx_01J00000000000000000000000".into()),
            runtime_context,
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
            work_run_id: Some(delivery.snapshot().work_run_aggregate.runs[0].id.clone()),
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

    #[test]
    fn verification_replacement_keeps_candidate_and_input_unchanged() {
        let mut snapshot = running_bound_delivery().into_snapshot();
        snapshot.session_bindings[0].execution_profile = Some("verifier".into());
        snapshot.session_bindings[0]
            .runtime_context
            .as_mut()
            .expect("verifier runtime context")
            .agent_identity
            .role = "verifier".into();
        snapshot.work_run_aggregate.items[0].state =
            winwincode_domain::WorkItemState::CandidateReady;
        let mut producer = snapshot.work_run_aggregate.runs[0].clone();
        producer.id.0 = "wrn_01J00000000000000000000008".into();
        producer.execution_job_id.0 = "job_01J00000000000000000000008".into();
        producer.state = winwincode_domain::WorkRunState::CandidateReady;
        producer.worker_session_id.0 = "wsn_01J00000000000000000000008".into();
        producer.product_session_id = Some(winwincode_domain::ProductSessionId(
            "psn_01J00000000000000000000008".into(),
        ));
        producer.codex_thread_id = Some(CodexThreadId("cdx_01J00000000000000000000008".into()));
        let mut producer_binding = snapshot.session_bindings[0].clone();
        producer_binding.id.0 = "binding-producer".into();
        producer_binding.lease_id.as_mut().unwrap().0 = "lse_01J00000000000000000000008".into();
        producer_binding.work_run_id = producer.id.clone();
        producer_binding.execution_job_id = producer.execution_job_id.clone();
        producer_binding.execution_profile = Some("executor".into());
        producer_binding
            .runtime_context
            .as_mut()
            .expect("producer runtime context")
            .agent_identity
            .role = "executor".into();
        producer_binding.worker_session_id = Some(producer.worker_session_id.clone());
        producer_binding.product_session_id = producer.product_session_id.clone().unwrap();
        producer_binding.codex_thread_id = producer.codex_thread_id.clone();
        snapshot.work_run_aggregate.runs.push(producer.clone());
        snapshot.session_bindings.push(producer_binding);
        let delivery = Delivery::try_from_snapshot(snapshot).expect("candidate and verifier");
        let identity = identity(&delivery);
        let mut replacement = replacement(&delivery);
        replacement.work_run_id = Some(winwincode_domain::WorkRunId(
            "wrn_01J00000000000000000000009".into(),
        ));
        let successor = &replacement.successor_lease;
        let authority = SessionBindingAuthority::from_execution_port(
            successor.worker_id.clone(),
            successor.worker_instance_id.clone(),
            successor.lease_id.clone(),
            successor.attempt,
            successor.fencing_token.clone(),
            WorkerSessionId("wsn_01J00000000000000000000009".into()),
            ExecutionMessageId("msg_01J00000000000000000000009".into()),
        );
        let next = accept_replacement_worker_session_with_authority(
            &delivery,
            delivery.revision(),
            &identity,
            &replacement,
            &authority,
            1_800_000_000_130,
        )
        .expect("read-only replacement");
        assert_eq!(
            next.snapshot().work_run_aggregate.items,
            delivery.snapshot().work_run_aggregate.items
        );
        assert_eq!(next.snapshot().work_run_aggregate.runs[1], producer);
        assert_eq!(next.snapshot().work_run_aggregate.runs[2].attempt, 2);
        assert_eq!(
            next.snapshot().work_run_aggregate.runs[2].state,
            winwincode_domain::WorkRunState::Leased
        );
    }

    #[test]
    fn replacement_acceptance_preserves_predecessor_and_appends_successor_binding() {
        let delivery = running_bound_delivery();
        let identity = identity(&delivery);
        let mut replacement = replacement(&delivery);
        let successor_id = winwincode_domain::WorkRunId("wrn_01J00000000000000000000009".into());
        replacement.work_run_id = Some(successor_id.clone());
        let successor = &replacement.successor_lease;
        let authority = SessionBindingAuthority::from_execution_port(
            successor.worker_id.clone(),
            successor.worker_instance_id.clone(),
            successor.lease_id.clone(),
            successor.attempt,
            successor.fencing_token.clone(),
            WorkerSessionId("wsn_01J00000000000000000000009".into()),
            ExecutionMessageId("msg_01J00000000000000000000009".into()),
        );
        let next = accept_replacement_worker_session_with_authority(
            &delivery,
            delivery.revision(),
            &identity,
            &replacement,
            &authority,
            1_800_000_000_130,
        )
        .expect("successor replacement");
        assert_eq!(next.snapshot().work_run_aggregate.runs.len(), 2);
        assert_eq!(next.snapshot().session_bindings.len(), 2);
        assert_eq!(
            next.snapshot().work_run_aggregate.runs[0].id,
            identity.work_run_id
        );
        assert_eq!(next.snapshot().work_run_aggregate.runs[0].attempt, 1);
        assert_eq!(
            next.snapshot().work_run_aggregate.runs[0].state,
            winwincode_domain::WorkRunState::Failed
        );
        assert_eq!(next.snapshot().work_run_aggregate.runs[1].id, successor_id);
        assert_eq!(next.snapshot().work_run_aggregate.runs[1].attempt, 2);
        assert_eq!(
            next.snapshot().session_bindings[0].work_run_id,
            identity.work_run_id
        );
        assert_eq!(
            next.snapshot().session_bindings[1].work_run_id,
            successor_id
        );

        let mut changed_run = delivery.clone().into_snapshot();
        changed_run.work_run_aggregate.runs[0].worker_instance_id =
            WorkerInstanceId("wki_01J00000000000000000000008".into());
        let changed_run = Delivery::try_from_snapshot(changed_run).expect("valid identity shape");
        assert!(
            accept_replacement_worker_session_with_authority(
                &changed_run,
                changed_run.revision(),
                &identity,
                &replacement,
                &authority,
                1_800_000_000_130,
            )
            .is_err(),
            "a different persisted WorkRun instance must not borrow the binding authority"
        );

        let mut forged = replacement;
        forged.predecessor_lease.worker_instance_id =
            WorkerInstanceId("wki_01J00000000000000000000008".into());
        let error = accept_replacement_worker_session_with_authority(
            &delivery,
            delivery.revision(),
            &identity,
            &forged,
            &authority,
            1_800_000_000_130,
        )
        .expect_err("foreign predecessor authority");
        assert_eq!(error.code(), super::CoordinationErrorCode::BindingConflict);
    }
}
