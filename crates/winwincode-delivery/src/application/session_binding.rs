// SPDX-License-Identifier: Apache-2.0

//! Exact execution-session binding transitions.

use winwincode_domain::{
    CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, FencingToken, LeaseId,
    ProductSessionId, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
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
        SessionBindingAuthority, SessionBindingIdentity, accept_worker_session_with_authority,
    };
    use crate::domain::SessionBindingSourceProvenance;
    use crate::domain::{Delivery, DeliveryStatus, StageRunStatus, test_fixture};
    use winwincode_domain::{
        ExecutionMessageId, FencingToken, LeaseId, WorkerId, WorkerInstanceId, WorkerSessionId,
    };

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
