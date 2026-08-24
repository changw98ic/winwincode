// SPDX-License-Identifier: Apache-2.0

//! Delivery stage coordination application service.

use winwincode_domain::{
    AttentionItemId, DeliveryId, DeliveryTaskId, ExecutionJobId, FencingToken, LeaseId,
    ProductSessionId, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

use crate::domain::{
    AttentionItem, AttentionItemStatus, AttentionItemType, DELIVERY_SCHEMA_VERSION, Delivery,
    DeliverySnapshot, DeliveryStage, DeliveryStatus, DeliveryTaskStatus, SessionBinding,
    SessionBindingId, StageRun, StageRunActorType, StageRunStatus,
};

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
    pub execution_job_id: ExecutionJobId,
    pub attempt: u64,
    pub lease_id: LeaseId,
    pub fencing_token: FencingToken,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: WorkerSessionId,
}

/// Terminal fact reported by Worker through `ExecutionPort`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalWorkerOutcome {
    pub stage_run_id: StageRunId,
    pub execution_job_id: ExecutionJobId,
    pub attempt: u64,
    pub lease_id: LeaseId,
    pub fencing_token: FencingToken,
    pub worker_id: WorkerId,
    pub worker_instance_id: WorkerInstanceId,
    pub worker_session_id: WorkerSessionId,
    pub status: TerminalOutcomeStatus,
}

/// A terminal fact that matched the current `StageRun`, `SessionBinding`, and
/// scheduler lease/fencing identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedTerminalOutcome {
    stage_run_id: StageRunId,
    lease_identity: ActiveLeaseIdentity,
    status: TerminalOutcomeStatus,
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
}

#[cfg(test)]
pub(crate) fn fixture_verified_terminal_outcome(
    stage_run_id: StageRunId,
    lease_identity: ActiveLeaseIdentity,
    status: TerminalOutcomeStatus,
) -> VerifiedTerminalOutcome {
    VerifiedTerminalOutcome {
        stage_run_id,
        lease_identity,
        status,
    }
}

/// Verifies a Worker terminal outcome against both Delivery and scheduler facts.
///
/// # Errors
///
/// Fails closed when any Delivery, job, attempt, Worker, lease, instance, or
/// fencing identity differs.
pub fn verify_terminal_outcome(
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageAdvanceEffect {
    Dispatch(ExecutionIntent),
    Review(AttentionItemId),
    Resume(ExecutionIntent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageAdvanceResult {
    pub delivery: Delivery,
    pub effect: StageAdvanceEffect,
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
    settle_previous_run(&mut snapshot, next.previous, input.now_millis)?;
    start_selected_task(&mut snapshot, &next)?;
    if next.stage == DeliveryStage::Reworking {
        crate::domain::rework::invalidate_candidate_authorization_for_writer_start(&mut snapshot);
    }
    snapshot.stage_runs.push(run);
    snapshot.updated_at_millis = input.now_millis;
    let effect = append_stage_effect(delivery, &mut snapshot, &next, input)?;
    let delivery = Delivery::try_from_snapshot(snapshot).map_err(|error| {
        CoordinationError::new(CoordinationErrorCode::Conflict, error.to_string())
    })?;
    Ok(StageAdvanceResult { delivery, effect })
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
    )?;
    let delivery_task_id = select_task_id(delivery, stage, previous)?;
    let role = role_for_stage(delivery, stage, previous, delivery_task_id.as_ref())?;
    validate_stage_executor(stage, actor_type, role)?;
    let attempt = delivery
        .snapshot()
        .stage_runs
        .iter()
        .filter(|run| {
            run.stage == stage && run.role == role && run.delivery_task_id == delivery_task_id
        })
        .count() as u64
        + 1;
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

fn validate_previous_outcome(
    delivery: &Delivery,
    previous: Option<&StageRun>,
    outcome: Option<&VerifiedTerminalOutcome>,
    current_lease: Option<&ActiveLeaseIdentity>,
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
        && outcome.lease_identity.attempt == previous.attempt
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
    finished_at_millis: u64,
) -> Result<(), CoordinationError> {
    if let Some(previous) = previous {
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
    Ok(StageAdvanceEffect::Dispatch(ExecutionIntent {
        execution_job_id: input.identities.execution_job_id,
        product_session_id: input.product_session_id,
        delivery_id: delivery.id().clone(),
        delivery_task_id: next.delivery_task_id.clone(),
        stage_run_id: input.identities.stage_run_id,
        stage: next.stage,
        role: next.role.to_owned(),
        attempt: next.attempt,
        goal: goal_for_task(delivery, next.delivery_task_id.as_ref()),
    }))
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
    Ok(StageAdvanceResult {
        delivery: delivery.clone(),
        effect: StageAdvanceEffect::Resume(ExecutionIntent {
            execution_job_id: binding.execution_job_id.clone(),
            product_session_id: binding.product_session_id.clone(),
            delivery_id: run.delivery_id.clone(),
            delivery_task_id: run.delivery_task_id.clone(),
            stage_run_id: run.id.clone(),
            stage: run.stage,
            role: run.role.clone(),
            attempt: run.attempt,
            goal,
        }),
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
    finished_at_millis: u64,
) -> Result<Delivery, CoordinationError> {
    if delivery.revision() != expected_revision {
        return Err(CoordinationError::new(
            CoordinationErrorCode::RevisionConflict,
            "Delivery revision changed before terminal outcome",
        ));
    }
    require_mutation_time(delivery, finished_at_millis)?;
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
        || finished_at_millis < run.started_at_millis
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
    stored_run.finished_at_millis = Some(finished_at_millis);
    if outcome.status != TerminalOutcomeStatus::Succeeded
        && let Some(task_id) = task_id
    {
        restore_task_after_unsuccessful_outcome(&mut snapshot, &task_id, run_stage)?;
    }
    snapshot.revision += 1;
    snapshot.updated_at_millis = finished_at_millis;
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
