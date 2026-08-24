use std::sync::Arc;

use winwincode_delivery::{
    application::CoordinationErrorCode,
    application::attention::{AttentionDecision, ResolveAttentionInput, resolve_attention},
    application::session_binding::{
        SessionBindingIdentity, accept_worker_session, report_codex_thread,
    },
    application::stage::{
        ActiveLeaseIdentity, AdvanceStageInput, CancelAcknowledgement, NewStageIdentities,
        ReviewAttentionSeed, StageAdvanceEffect, TerminalOutcomeStatus, TerminalWorkerOutcome,
        acknowledge_cancel, advance, apply_cancelled_outcome, request_cancel, resume_active,
        validate_stage_executor, verify_terminal_outcome,
    },
    application::task::{
        TaskFact, approve_task_breakdown, runnable_task, transition_task_status,
        validate_create_tasks_empty,
    },
    domain::{
        Delivery, DeliveryStage, DeliveryStatus, DeliveryTask, DeliveryTaskStatus,
        StageRunActorType,
    },
    store::{
        AppendDelivery, CreateDelivery, DeliveryCommand, DeliveryCommandPort,
        DeliveryMutationOperation, DeliveryStore, InMemoryDeliveryJournal,
    },
};
use winwincode_domain::{
    AttentionItemId, CodexThreadId, DeliveryTaskId, ExecutionJobId, FencingToken, LeaseId,
    ProductSessionId, RequestId, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

fn delivery_with_status(status: DeliveryStatus) -> Delivery {
    let mut snapshot = Delivery::decode_json(include_bytes!("fixtures/delivery-main.json"))
        .expect("canonical fixture")
        .into_snapshot();
    snapshot.revision = 1;
    snapshot.status = status;
    snapshot.tasks.clear();
    snapshot.stage_runs.clear();
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.updated_at_millis = snapshot.created_at_millis;
    Delivery::try_from_snapshot(snapshot).expect("test Delivery")
}

fn advance_input(expected_revision: u64, suffix: &str) -> AdvanceStageInput {
    AdvanceStageInput {
        expected_revision,
        product_session_id: ProductSessionId(format!("product-session-{suffix}")),
        identities: NewStageIdentities {
            stage_run_id: StageRunId(format!("stage-{suffix}")),
            execution_job_id: ExecutionJobId(format!("job-{suffix}")),
            session_binding_id: winwincode_delivery::domain::SessionBindingId::new(format!(
                "binding-{suffix}"
            ))
            .expect("binding id"),
            attention_item_id: AttentionItemId(format!("attention-{suffix}")),
        },
        review: None,
        previous_outcome: None,
        now_millis: 1_800_000_000_100,
    }
}

fn completed_active_binding(delivery: Delivery, suffix: &str) -> Delivery {
    let mut snapshot = delivery.into_snapshot();
    let binding = snapshot
        .session_bindings
        .last_mut()
        .expect("active binding");
    binding.worker_session_id = Some(WorkerSessionId(format!("worker-session-{suffix}")));
    binding.codex_thread_id = Some(CodexThreadId(format!("codex-thread-{suffix}")));
    Delivery::try_from_snapshot(snapshot).expect("completed binding")
}

fn approved_plan_without_tasks() -> Delivery {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "approved-plan"))
            .expect("planning starts")
            .delivery,
        "approved-plan",
    );
    let mut input = advance_input(2, "approved-plan-review");
    input.review = Some(ReviewAttentionSeed {
        title: "Approve plan".into(),
        context: "frozen-plan-review-context".into(),
        assigned_to: "reviewer-one".into(),
    });
    input.previous_outcome = Some(successful_outcome(&planning));
    let review = advance(&planning, input).expect("review starts").delivery;
    let mut snapshot = review.into_snapshot();
    let run = snapshot.stage_runs.last_mut().expect("review run");
    run.status = winwincode_delivery::domain::StageRunStatus::Succeeded;
    run.finished_at_millis = Some(1_800_000_000_150);
    let item = snapshot
        .attention_items
        .last_mut()
        .expect("review Attention");
    item.status = winwincode_delivery::domain::AttentionItemStatus::Resolved;
    item.resolution = Some("approved current plan".into());
    item.resolved_by = Some("reviewer-one".into());
    item.resolved_at_millis = Some(1_800_000_000_150);
    snapshot.status = DeliveryStatus::Executing;
    snapshot.revision += 1;
    snapshot.updated_at_millis = 1_800_000_000_150;
    Delivery::try_from_snapshot(snapshot).expect("approved plan fixture")
}

fn task(delivery: &Delivery, id: &str, blocked_by: Vec<DeliveryTaskId>) -> DeliveryTask {
    DeliveryTask {
        schema_version: winwincode_delivery::domain::DELIVERY_SCHEMA_VERSION,
        id: DeliveryTaskId(id.into()),
        delivery_id: delivery.id().clone(),
        title: format!("Task {id}"),
        goal: format!("Complete {id}"),
        acceptance_criterion_ids: vec![delivery.snapshot().spec.acceptance_criteria[0].id.clone()],
        blocked_by_task_ids: blocked_by,
        owner: None,
        status: DeliveryTaskStatus::Pending,
    }
}

fn verified_outcome(
    delivery: &Delivery,
    status: TerminalOutcomeStatus,
) -> winwincode_delivery::application::stage::VerifiedTerminalOutcome {
    let run = delivery
        .snapshot()
        .stage_runs
        .iter()
        .find(|run| {
            matches!(
                run.status,
                winwincode_delivery::domain::StageRunStatus::Running
            )
        })
        .expect("active run");
    let binding = delivery
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.stage_run_id == run.id)
        .expect("active binding");
    let worker_session_id = binding
        .worker_session_id
        .clone()
        .expect("completed worker binding");
    let lease = ActiveLeaseIdentity {
        execution_job_id: binding.execution_job_id.clone(),
        attempt: run.attempt,
        lease_id: LeaseId(format!("lease-{}", run.id.0)),
        fencing_token: FencingToken("1".into()),
        worker_id: WorkerId("worker-one".into()),
        worker_instance_id: WorkerInstanceId("worker-instance-one".into()),
        worker_session_id: worker_session_id.clone(),
    };
    verify_terminal_outcome(
        delivery,
        &lease,
        TerminalWorkerOutcome {
            stage_run_id: run.id.clone(),
            execution_job_id: binding.execution_job_id.clone(),
            attempt: run.attempt,
            lease_id: lease.lease_id.clone(),
            fencing_token: lease.fencing_token.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
            worker_session_id,
            status,
        },
    )
    .expect("verified successful outcome")
}

fn successful_outcome(
    delivery: &Delivery,
) -> winwincode_delivery::application::stage::VerifiedTerminalOutcome {
    verified_outcome(delivery, TerminalOutcomeStatus::Succeeded)
}

#[test]
fn advance_selects_only_the_legal_next_stage() {
    let draft = delivery_with_status(DeliveryStatus::Draft);
    let result = advance(&draft, advance_input(1, "clarifying")).expect("legal advance");
    let run = result
        .delivery
        .snapshot()
        .stage_runs
        .last()
        .expect("new StageRun");

    assert_eq!(run.stage, DeliveryStage::Clarifying);
    assert_eq!(run.actor_type, StageRunActorType::Codex);
    assert_eq!(run.role, "requirements");
    assert_eq!(run.attempt, 1);
    assert!(matches!(result.effect, StageAdvanceEffect::Dispatch(_)));

    let ready = delivery_with_status(DeliveryStatus::Ready);
    let result = advance(&ready, advance_input(1, "planning")).expect("legal advance");
    assert_eq!(
        result.delivery.snapshot().stage_runs[0].stage,
        DeliveryStage::Planning
    );
    assert_eq!(result.delivery.snapshot().status, DeliveryStatus::Planning);
}

#[test]
fn stage_advance_rejects_time_before_current_delivery_state() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let mut snapshot = ready.into_snapshot();
    snapshot.updated_at_millis += 100;
    let ready = Delivery::try_from_snapshot(snapshot).expect("Delivery with a later update");
    let mut input = advance_input(1, "backdated-stage");
    input.now_millis = ready.snapshot().updated_at_millis - 50;

    let error =
        advance(&ready, input).expect_err("stage start cannot move Delivery time backwards");

    assert_eq!(error.code(), CoordinationErrorCode::InvalidRequest);
    assert!(ready.snapshot().stage_runs.is_empty());
}

#[test]
fn advance_rejects_when_more_than_one_stage_run_is_active() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let started = advance(&ready, advance_input(1, "first")).expect("first stage");
    let mut snapshot = started.delivery.into_snapshot();
    let mut second_run = snapshot.stage_runs[0].clone();
    second_run.id = StageRunId("stage-second-active".into());
    let mut second_binding = snapshot.session_bindings[0].clone();
    second_binding.id = winwincode_delivery::domain::SessionBindingId::new("binding-second-active")
        .expect("binding id");
    second_binding.stage_run_id = second_run.id.clone();
    second_binding.execution_job_id = ExecutionJobId("job-second-active".into());
    snapshot.stage_runs.push(second_run);
    snapshot.session_bindings.push(second_binding);
    let conflicted = Delivery::try_from_snapshot(snapshot).expect("legacy conflict fixture");

    let error = advance(&conflicted, advance_input(2, "third"))
        .expect_err("multiple active StageRuns must fail closed");
    assert_eq!(error.code(), CoordinationErrorCode::Conflict);
}

#[test]
fn stage_actor_and_role_policy_rejects_wrong_executor() {
    assert!(
        validate_stage_executor(
            DeliveryStage::Verifying,
            StageRunActorType::Codex,
            "verifier"
        )
        .is_ok()
    );
    assert!(
        validate_stage_executor(
            DeliveryStage::Verifying,
            StageRunActorType::Codex,
            "executor"
        )
        .is_err()
    );
    assert!(
        validate_stage_executor(
            DeliveryStage::PlanReview,
            StageRunActorType::Codex,
            "reviewer"
        )
        .is_err()
    );
    assert!(
        validate_stage_executor(
            DeliveryStage::Reworking,
            StageRunActorType::Codex,
            "remediator"
        )
        .is_ok()
    );
    assert!(
        validate_stage_executor(
            DeliveryStage::Reworking,
            StageRunActorType::Human,
            "remediator"
        )
        .is_err()
    );
}

#[test]
fn recovery_rejects_stage_with_foreign_actor_or_role() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let started = advance(&ready, advance_input(1, "foreign-role"))
        .expect("planning starts")
        .delivery;
    let mut snapshot = started.into_snapshot();
    snapshot.stage_runs[0].role = "executor".into();
    let corrupted = Delivery::try_from_snapshot(snapshot)
        .expect("legacy snapshot reaches application policy validation");

    let error = resume_active(&corrupted, corrupted.revision())
        .expect_err("recovery must not resume a foreign stage role");
    assert_eq!(error.code(), CoordinationErrorCode::InvalidRequest);
}

#[test]
fn starting_next_stage_settles_bound_previous_run_atomically() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = advance(&ready, advance_input(1, "planning-handoff"))
        .expect("planning starts")
        .delivery;
    let planning = completed_active_binding(planning, "planning-handoff");
    let planning_run_id = planning.snapshot().stage_runs[0].id.clone();
    let mut input = advance_input(2, "plan-review-handoff");
    input.review = Some(ReviewAttentionSeed {
        title: "Review the current plan".into(),
        context: "frozen-plan-review-context-v1".into(),
        assigned_to: "reviewer-one".into(),
    });
    input.previous_outcome = Some(successful_outcome(&planning));

    let result = advance(&planning, input).expect("plan review handoff");
    let snapshot = result.delivery.snapshot();
    let previous = snapshot
        .stage_runs
        .iter()
        .find(|run| run.id == planning_run_id)
        .expect("planning run");
    let review = snapshot.stage_runs.last().expect("review run");

    assert_eq!(
        previous.status,
        winwincode_delivery::domain::StageRunStatus::Succeeded
    );
    assert_eq!(previous.finished_at_millis, Some(1_800_000_000_100));
    assert_eq!(review.stage, DeliveryStage::PlanReview);
    assert_eq!(
        review.status,
        winwincode_delivery::domain::StageRunStatus::Waiting
    );
    assert_eq!(snapshot.status, DeliveryStatus::NeedsAttention);
    assert!(matches!(result.effect, StageAdvanceEffect::Review(_)));
}

#[test]
fn review_stage_opens_linked_blocking_attention_atomically() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "planning-review"))
            .expect("planning starts")
            .delivery,
        "planning-review",
    );
    let mut input = advance_input(2, "review-attention");
    input.review = Some(ReviewAttentionSeed {
        title: "Approve the task plan".into(),
        context: "frozen-review-set:abc123".into(),
        assigned_to: "reviewer-one".into(),
    });
    input.previous_outcome = Some(successful_outcome(&planning));

    let result = advance(&planning, input).expect("review starts");
    let snapshot = result.delivery.snapshot();
    let run = snapshot.stage_runs.last().expect("review StageRun");
    let item = snapshot.attention_items.last().expect("review Attention");

    assert_eq!(item.stage_run_id.as_ref(), Some(&run.id));
    assert!(item.blocking);
    assert_eq!(
        item.status,
        winwincode_delivery::domain::AttentionItemStatus::Open
    );
    assert_eq!(item.context, "frozen-review-set:abc123");
    assert_eq!(snapshot.revision, 3);
}

#[test]
fn active_stage_run_resumes_without_new_run_or_attempt() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "resume"))
            .expect("planning starts")
            .delivery,
        "resume",
    );
    let revision = planning.revision();
    let run = planning.snapshot().stage_runs[0].clone();
    let binding = planning.snapshot().session_bindings[0].clone();

    let resumed = resume_active(&planning, revision).expect("active stage resumes");
    assert_eq!(resumed.delivery, planning);
    assert_eq!(resumed.delivery.revision(), revision);
    assert_eq!(
        resumed.delivery.snapshot().stage_runs,
        std::slice::from_ref(&run)
    );
    let StageAdvanceEffect::Resume(intent) = resumed.effect else {
        panic!("resume must return the existing ExecutionIntent");
    };
    assert_eq!(intent.stage_run_id, run.id);
    assert_eq!(intent.execution_job_id, binding.execution_job_id);
    assert_eq!(intent.attempt, run.attempt);
}

#[test]
fn active_stage_run_resumes_before_worker_acceptance_without_new_identity() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = advance(&ready, advance_input(1, "resume-before-worker"))
        .expect("planning starts")
        .delivery;
    let revision = planning.revision();
    let run = planning.snapshot().stage_runs[0].clone();
    let binding = planning.snapshot().session_bindings[0].clone();
    assert!(binding.worker_session_id.is_none());
    assert!(binding.codex_thread_id.is_none());

    let resumed = resume_active(&planning, revision)
        .expect("durable job resumes before Worker accepts dispatch");

    assert_eq!(resumed.delivery, planning);
    assert_eq!(resumed.delivery.revision(), revision);
    assert_eq!(
        resumed.delivery.snapshot().stage_runs.as_slice(),
        std::slice::from_ref(&run)
    );
    let StageAdvanceEffect::Resume(intent) = resumed.effect else {
        panic!("resume must return the original pending ExecutionIntent");
    };
    assert_eq!(intent.stage_run_id, run.id);
    assert_eq!(intent.execution_job_id, binding.execution_job_id);
    assert_eq!(intent.attempt, run.attempt);
}

#[test]
fn cancel_request_waits_for_terminal_job_outcome() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "cancel-waits"))
            .expect("planning starts")
            .delivery,
        "cancel-waits",
    );
    let cancel = request_cancel(&planning, planning.revision()).expect("cancel intent");
    let after_ack = acknowledge_cancel(
        &planning,
        &cancel,
        &CancelAcknowledgement {
            stage_run_id: cancel.stage_run_id.clone(),
            execution_job_id: cancel.execution_job_id.clone(),
            attempt: cancel.attempt,
            worker_session_id: cancel.worker_session_id.clone(),
        },
    )
    .expect("matching ack");

    assert_eq!(after_ack, planning);
    assert_eq!(after_ack.revision(), 2);
    assert_eq!(
        after_ack.snapshot().stage_runs[0].status,
        winwincode_delivery::domain::StageRunStatus::Running
    );
    assert!(
        after_ack.snapshot().stage_runs[0]
            .finished_at_millis
            .is_none()
    );
}

#[test]
fn cancelled_outcome_settles_the_same_stage_run() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "cancel-terminal"))
            .expect("planning starts")
            .delivery,
        "cancel-terminal",
    );
    let original_run = planning.snapshot().stage_runs[0].clone();
    let outcome = verified_outcome(&planning, TerminalOutcomeStatus::Cancelled);

    let cancelled =
        apply_cancelled_outcome(&planning, planning.revision(), &outcome, 1_800_000_000_200)
            .expect("terminal cancellation settles the run");

    assert_eq!(cancelled.snapshot().stage_runs.len(), 1);
    assert_eq!(cancelled.snapshot().stage_runs[0].id, original_run.id);
    assert_eq!(
        cancelled.snapshot().stage_runs[0].status,
        winwincode_delivery::domain::StageRunStatus::Cancelled
    );
    assert_eq!(
        cancelled.snapshot().stage_runs[0].finished_at_millis,
        Some(1_800_000_000_200)
    );
    assert_eq!(cancelled.revision(), planning.revision() + 1);
}

#[test]
fn task_breakdown_approval_replaces_empty_graph_once() {
    let approved_plan = approved_plan_without_tasks();
    let first = task(&approved_plan, "task-first", vec![]);
    let second = task(&approved_plan, "task-second", vec![first.id.clone()]);

    let with_tasks = approve_task_breakdown(
        &approved_plan,
        approved_plan.revision(),
        vec![first.clone(), second.clone()],
        1_800_000_000_200,
    )
    .expect("current reviewed graph is approved once");
    assert_eq!(with_tasks.snapshot().tasks, [first, second]);
    assert_eq!(with_tasks.revision(), approved_plan.revision() + 1);

    let error = approve_task_breakdown(
        &with_tasks,
        with_tasks.revision(),
        vec![task(&with_tasks, "task-replacement", vec![])],
        1_800_000_000_300,
    )
    .expect_err("the same Spec revision cannot replace its task graph");
    assert_eq!(error.code(), CoordinationErrorCode::Conflict);
}

#[test]
fn approved_plan_without_a_frozen_task_graph_cannot_start_execution() {
    let approved_plan = approved_plan_without_tasks();
    let mut input = advance_input(approved_plan.revision(), "empty-task-graph");
    input.now_millis = 1_800_000_000_200;

    let error = advance(&approved_plan, input)
        .expect_err("execution must wait for a non-empty approved task graph");

    assert_eq!(error.code(), CoordinationErrorCode::WrongState);
    assert!(
        approved_plan
            .snapshot()
            .stage_runs
            .last()
            .is_some_and(|run| {
                run.stage == DeliveryStage::PlanReview
                    && run.status == winwincode_delivery::domain::StageRunStatus::Succeeded
            })
    );
}

#[test]
fn delivery_create_accepts_only_an_empty_task_graph() {
    let delivery = approved_plan_without_tasks();

    assert!(validate_create_tasks_empty(&[]).is_ok());
    assert!(
        validate_create_tasks_empty(&[task(&delivery, "task-create-rejected", vec![])]).is_err()
    );
}

#[test]
fn task_breakdown_rejects_missing_self_and_cyclic_dependencies() {
    let delivery = approved_plan_without_tasks();

    let missing = task(
        &delivery,
        "task-missing",
        vec![DeliveryTaskId("task-does-not-exist".into())],
    );
    assert!(
        approve_task_breakdown(
            &delivery,
            delivery.revision(),
            vec![missing],
            1_800_000_000_200,
        )
        .is_err()
    );

    let self_id = DeliveryTaskId("task-self".into());
    let self_referencing = task(&delivery, "task-self", vec![self_id]);
    assert!(
        approve_task_breakdown(
            &delivery,
            delivery.revision(),
            vec![self_referencing],
            1_800_000_000_200,
        )
        .is_err()
    );

    let first_id = DeliveryTaskId("task-cycle-first".into());
    let second_id = DeliveryTaskId("task-cycle-second".into());
    let first = task(&delivery, &first_id.0, vec![second_id.clone()]);
    let second = task(&delivery, &second_id.0, vec![first_id]);
    assert!(
        approve_task_breakdown(
            &delivery,
            delivery.revision(),
            vec![first, second],
            1_800_000_000_200,
        )
        .is_err()
    );
}

#[test]
fn blocked_task_never_becomes_runnable() {
    let approved = approved_plan_without_tasks();
    let dependency = task(&approved, "task-dependency", vec![]);
    let blocked = task(&approved, "task-blocked", vec![dependency.id.clone()]);
    let with_tasks = approve_task_breakdown(
        &approved,
        approved.revision(),
        vec![blocked.clone(), dependency.clone()],
        1_800_000_000_200,
    )
    .expect("valid graph");

    assert_eq!(
        runnable_task(&with_tasks, DeliveryStage::Executing)
            .expect("one runnable task")
            .id,
        dependency.id
    );
    assert_ne!(
        runnable_task(&with_tasks, DeliveryStage::Executing)
            .expect("one runnable task")
            .id,
        blocked.id
    );
}

#[test]
fn task_status_tracks_execution_verification_rework_and_cancel() {
    assert_eq!(
        transition_task_status(DeliveryTaskStatus::Pending, TaskFact::StartExecuting)
            .expect("execution starts"),
        DeliveryTaskStatus::Active
    );
    assert_eq!(
        transition_task_status(DeliveryTaskStatus::Active, TaskFact::StartVerifying)
            .expect("verification starts"),
        DeliveryTaskStatus::Verifying
    );
    assert_eq!(
        transition_task_status(DeliveryTaskStatus::Verifying, TaskFact::VerificationPassed)
            .expect("verification passes"),
        DeliveryTaskStatus::Completed
    );
    assert_eq!(
        transition_task_status(DeliveryTaskStatus::Verifying, TaskFact::VerificationFailed)
            .expect("verification fails"),
        DeliveryTaskStatus::Failed
    );
    assert_eq!(
        transition_task_status(DeliveryTaskStatus::Failed, TaskFact::StartReworking)
            .expect("rework starts"),
        DeliveryTaskStatus::Active
    );
    assert_eq!(
        transition_task_status(DeliveryTaskStatus::Active, TaskFact::ExecutingCancelled)
            .expect("execution cancellation"),
        DeliveryTaskStatus::Pending
    );
    assert_eq!(
        transition_task_status(DeliveryTaskStatus::Verifying, TaskFact::VerifyingCancelled)
            .expect("verification cancellation"),
        DeliveryTaskStatus::Verifying
    );
    assert_eq!(
        transition_task_status(DeliveryTaskStatus::Active, TaskFact::ReworkingCancelled)
            .expect("rework cancellation"),
        DeliveryTaskStatus::Failed
    );
    assert!(
        transition_task_status(DeliveryTaskStatus::Pending, TaskFact::VerificationPassed).is_err()
    );

    let approved = approved_plan_without_tasks();
    let approved_task = task(&approved, "task-integrated-status", vec![]);
    let with_tasks = approve_task_breakdown(
        &approved,
        approved.revision(),
        vec![approved_task.clone()],
        1_800_000_000_200,
    )
    .expect("approve task graph");
    let mut execution_input = advance_input(with_tasks.revision(), "task-integrated-execution");
    execution_input.now_millis = 1_800_000_000_300;
    let executing = advance(&with_tasks, execution_input)
        .expect("task execution starts")
        .delivery;
    assert_eq!(
        executing.snapshot().tasks[0].status,
        DeliveryTaskStatus::Active
    );
    assert_eq!(
        executing
            .snapshot()
            .stage_runs
            .last()
            .expect("execution")
            .delivery_task_id,
        Some(approved_task.id)
    );

    let executing = completed_active_binding(executing, "task-integrated-execution");
    let mut verification_input =
        advance_input(executing.revision(), "task-integrated-verification");
    verification_input.previous_outcome = Some(successful_outcome(&executing));
    verification_input.now_millis = 1_800_000_000_400;
    let verifying = advance(&executing, verification_input)
        .expect("task verification starts")
        .delivery;
    assert_eq!(
        verifying.snapshot().tasks[0].status,
        DeliveryTaskStatus::Verifying
    );
    assert_eq!(
        verifying
            .snapshot()
            .stage_runs
            .last()
            .expect("verification")
            .role,
        "reviewer"
    );
}

#[test]
fn session_binding_matches_exact_delivery_task_stage_job_and_session_identities() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let started = advance(&ready, advance_input(1, "exact-binding"))
        .expect("planning starts")
        .delivery;
    let binding = started.snapshot().session_bindings[0].clone();
    let identity = SessionBindingIdentity {
        delivery_id: started.id().clone(),
        delivery_task_id: None,
        stage_run_id: started.snapshot().stage_runs[0].id.clone(),
        product_session_id: binding.product_session_id.clone(),
        execution_job_id: binding.execution_job_id.clone(),
    };
    let worker_session_id = WorkerSessionId("worker-session-exact".into());
    let worker_bound = accept_worker_session(
        &started,
        started.revision(),
        &identity,
        worker_session_id.clone(),
        1_800_000_000_120,
    )
    .expect("Worker accepts exact dispatch");
    let codex_thread_id = CodexThreadId("codex-thread-exact".into());
    let complete = report_codex_thread(
        &worker_bound,
        worker_bound.revision(),
        &identity,
        &worker_session_id,
        codex_thread_id.clone(),
        1_800_000_000_130,
    )
    .expect("Worker reports exact Codex thread");
    let stored = &complete.snapshot().session_bindings[0];

    assert_eq!(stored.delivery_id, identity.delivery_id);
    assert_eq!(stored.delivery_task_id, identity.delivery_task_id);
    assert_eq!(stored.stage_run_id, identity.stage_run_id);
    assert_eq!(stored.product_session_id, identity.product_session_id);
    assert_eq!(stored.execution_job_id, identity.execution_job_id);
    assert_eq!(stored.worker_session_id, Some(worker_session_id));
    assert_eq!(stored.codex_thread_id, Some(codex_thread_id));
}

#[test]
fn conflicting_session_binding_stops_recovery() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let started = advance(&ready, advance_input(1, "binding-conflict"))
        .expect("planning starts")
        .delivery;
    let resumed = resume_active(&started, started.revision())
        .expect("pending dispatch resumes before Worker acceptance");
    let StageAdvanceEffect::Resume(intent) = resumed.effect else {
        panic!("pending dispatch must resume the same execution intent");
    };
    assert_eq!(intent.stage_run_id, started.snapshot().stage_runs[0].id);
    assert_eq!(
        intent.execution_job_id,
        started.snapshot().session_bindings[0].execution_job_id
    );

    let binding = &started.snapshot().session_bindings[0];
    let wrong_identity = SessionBindingIdentity {
        delivery_id: started.id().clone(),
        delivery_task_id: None,
        stage_run_id: started.snapshot().stage_runs[0].id.clone(),
        product_session_id: ProductSessionId("product-session-foreign".into()),
        execution_job_id: binding.execution_job_id.clone(),
    };
    let error = accept_worker_session(
        &started,
        started.revision(),
        &wrong_identity,
        WorkerSessionId("worker-session-foreign".into()),
        1_800_000_000_120,
    )
    .expect_err("foreign ProductSession must stop recovery");
    assert_eq!(error.code(), CoordinationErrorCode::BindingConflict);
    assert!(
        started.snapshot().session_bindings[0]
            .worker_session_id
            .is_none()
    );
}

#[test]
fn open_blocking_attention_prevents_stage_advance() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "attention-block"))
            .expect("planning starts")
            .delivery,
        "attention-block",
    );
    let mut input = advance_input(2, "attention-review");
    input.review = Some(ReviewAttentionSeed {
        title: "Review".into(),
        context: "frozen-attention".into(),
        assigned_to: "reviewer-one".into(),
    });
    input.previous_outcome = Some(successful_outcome(&planning));
    let review = advance(&planning, input).expect("review starts").delivery;

    let error = advance(&review, advance_input(review.revision(), "bypass"))
        .expect_err("open blocker prevents dispatch");
    assert_eq!(error.code(), CoordinationErrorCode::AttentionRequired);
}

#[test]
fn stale_attention_resolution_is_rejected_without_state_change() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "stale-attention"))
            .expect("planning starts")
            .delivery,
        "stale-attention",
    );
    let mut input = advance_input(2, "stale-review");
    input.review = Some(ReviewAttentionSeed {
        title: "Review".into(),
        context: "frozen-current-context".into(),
        assigned_to: "reviewer-one".into(),
    });
    input.previous_outcome = Some(successful_outcome(&planning));
    let review = advance(&planning, input).expect("review starts").delivery;
    let item = review.snapshot().attention_items[0].clone();
    let before = review.clone();

    let error = resolve_attention(
        &review,
        ResolveAttentionInput {
            expected_revision: review.revision() - 1,
            attention_item_id: item.id.clone(),
            stage_run_id: item.stage_run_id.clone().expect("review run"),
            expected_context: "frozen-current-context".into(),
            actor: "reviewer-one".into(),
            decision: AttentionDecision::Resolved,
            resolution: "approved".into(),
            now_millis: 1_800_000_000_200,
        },
    )
    .expect_err("stale revision must fail");
    assert_eq!(error.code(), CoordinationErrorCode::RevisionConflict);
    assert_eq!(review, before);

    let mut wrong_context = ResolveAttentionInput {
        expected_revision: review.revision(),
        attention_item_id: item.id,
        stage_run_id: item.stage_run_id.expect("review run"),
        expected_context: "stale-frozen-context".into(),
        actor: "reviewer-one".into(),
        decision: AttentionDecision::Resolved,
        resolution: "approved".into(),
        now_millis: 1_800_000_000_200,
    };
    assert!(resolve_attention(&review, wrong_context.clone()).is_err());
    wrong_context.expected_context = "frozen-current-context".into();
    wrong_context.actor = "another-reviewer".into();
    assert!(resolve_attention(&review, wrong_context).is_err());
}

#[test]
fn resolving_one_of_multiple_blockers_keeps_delivery_blocked() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "multiple-blockers"))
            .expect("planning starts")
            .delivery,
        "multiple-blockers",
    );
    let mut input = advance_input(2, "multiple-review");
    input.review = Some(ReviewAttentionSeed {
        title: "First review".into(),
        context: "frozen-first-context".into(),
        assigned_to: "reviewer-one".into(),
    });
    input.previous_outcome = Some(successful_outcome(&planning));
    let review = advance(&planning, input).expect("review starts").delivery;
    let mut snapshot = review.into_snapshot();
    let mut second = snapshot.attention_items[0].clone();
    second.id = AttentionItemId("attention-second-blocker".into());
    second.title = "Second review".into();
    second.context = "frozen-second-context".into();
    snapshot.attention_items.push(second);
    let review = Delivery::try_from_snapshot(snapshot).expect("two current blockers");
    let first = review.snapshot().attention_items[0].clone();

    let resolved = resolve_attention(
        &review,
        ResolveAttentionInput {
            expected_revision: review.revision(),
            attention_item_id: first.id,
            stage_run_id: first.stage_run_id.expect("review run"),
            expected_context: first.context,
            actor: "reviewer-one".into(),
            decision: AttentionDecision::Resolved,
            resolution: "first decision approved".into(),
            now_millis: 1_800_000_000_200,
        },
    )
    .expect("first blocker resolves");

    assert_eq!(resolved.snapshot().status, DeliveryStatus::NeedsAttention);
    assert_eq!(
        resolved
            .snapshot()
            .attention_items
            .iter()
            .filter(|item| item.blocking
                && item.status == winwincode_delivery::domain::AttentionItemStatus::Open)
            .count(),
        1
    );
    assert_eq!(resolved.snapshot().stage_runs.len(), 2);

    let remaining = resolved
        .snapshot()
        .attention_items
        .iter()
        .find(|item| item.status == winwincode_delivery::domain::AttentionItemStatus::Open)
        .expect("second blocker remains")
        .clone();
    let completed = resolve_attention(
        &resolved,
        ResolveAttentionInput {
            expected_revision: resolved.revision(),
            attention_item_id: remaining.id,
            stage_run_id: remaining.stage_run_id.expect("same review run"),
            expected_context: remaining.context,
            actor: "reviewer-one".into(),
            decision: AttentionDecision::Resolved,
            resolution: "second decision approved".into(),
            now_millis: 1_800_000_000_300,
        },
    )
    .expect("remaining current blocker can resolve");

    assert_eq!(completed.snapshot().status, DeliveryStatus::Executing);
    assert!(!completed.snapshot().attention_items.iter().any(|item| {
        item.blocking && item.status == winwincode_delivery::domain::AttentionItemStatus::Open
    }));
    assert_eq!(
        completed
            .snapshot()
            .stage_runs
            .last()
            .expect("review run")
            .status,
        winwincode_delivery::domain::StageRunStatus::Succeeded
    );
}

#[test]
fn replayed_advance_returns_original_stage_run_without_new_state() {
    let journal = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(journal);
    let draft = delivery_with_status(DeliveryStatus::Draft);
    store
        .execute(DeliveryCommand::Create(CreateDelivery {
            request_id: RequestId("request-create-replay".into()),
            request_digest: "a".repeat(64),
            snapshot: draft.clone(),
        }))
        .expect("seed Delivery");
    let advanced = advance(&draft, advance_input(1, "replay-stage"))
        .expect("stage advance")
        .delivery;
    let first = store
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: draft.id().clone(),
            request_id: RequestId("request-advance-replay".into()),
            request_digest: "b".repeat(64),
            operation: DeliveryMutationOperation::StageStarted,
            expected_revision: 1,
            snapshot: advanced.clone(),
        }))
        .expect("first append");
    let replay = store
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: draft.id().clone(),
            request_id: RequestId("request-advance-replay".into()),
            request_digest: "b".repeat(64),
            operation: DeliveryMutationOperation::StageStarted,
            expected_revision: 1,
            snapshot: advanced.clone(),
        }))
        .expect("identical request replays");

    let conflict = store
        .execute(DeliveryCommand::Append(AppendDelivery {
            delivery_id: draft.id().clone(),
            request_id: RequestId("request-advance-replay".into()),
            request_digest: "b".repeat(64),
            operation: DeliveryMutationOperation::StageStarted,
            expected_revision: advanced.revision(),
            snapshot: advanced,
        }))
        .expect_err("different expectedRevision is conflicting request reuse");

    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(replay.snapshot, first.snapshot);
    assert_eq!(replay.snapshot.snapshot().stage_runs.len(), 1);
    assert_eq!(replay.snapshot.snapshot().session_bindings.len(), 1);
    assert_eq!(
        conflict.code(),
        winwincode_delivery::store::DeliveryStoreErrorCode::RequestConflict
    );
}

#[test]
fn verification_progress_stops_after_required_roles_without_optional_adversary() {
    let approved = approved_plan_without_tasks();
    let approved_task = task(&approved, "task-verification-sequence", vec![]);
    let executing = approve_task_breakdown(
        &approved,
        approved.revision(),
        vec![approved_task],
        1_800_000_000_200,
    )
    .expect("task graph approved");
    let mut writer_input = advance_input(executing.revision(), "writer-sequence");
    writer_input.now_millis = 1_800_000_000_300;
    let writer = completed_active_binding(
        advance(&executing, writer_input)
            .expect("executor starts")
            .delivery,
        "writer-sequence",
    );

    let mut reviewer_input = advance_input(writer.revision(), "reviewer-sequence");
    reviewer_input.now_millis = 1_800_000_000_400;
    reviewer_input.previous_outcome = Some(successful_outcome(&writer));
    let reviewer = advance(&writer, reviewer_input)
        .expect("reviewer starts")
        .delivery;
    assert_eq!(
        reviewer
            .snapshot()
            .stage_runs
            .last()
            .expect("reviewer")
            .role,
        "reviewer"
    );
    let reviewer = completed_active_binding(reviewer, "reviewer-sequence");

    let mut verifier_input = advance_input(reviewer.revision(), "verifier-sequence");
    verifier_input.now_millis = 1_800_000_000_500;
    verifier_input.previous_outcome = Some(successful_outcome(&reviewer));
    let verifier = advance(&reviewer, verifier_input)
        .expect("verifier starts")
        .delivery;
    assert_eq!(
        verifier
            .snapshot()
            .stage_runs
            .last()
            .expect("verifier")
            .role,
        "verifier"
    );
    let verifier = completed_active_binding(verifier, "verifier-sequence");

    let mut beyond = advance_input(verifier.revision(), "verification-overrun");
    beyond.now_millis = 1_800_000_000_600;
    beyond.previous_outcome = Some(successful_outcome(&verifier));
    let error =
        advance(&verifier, beyond).expect_err("optional adversarial verifier must not be forced");
    assert_eq!(error.code(), CoordinationErrorCode::WrongState);
    assert_eq!(verifier.snapshot().stage_runs.len(), 5);
}

#[test]
fn active_stage_handoff_requires_exact_terminal_lease_and_fencing_fact() {
    let ready = delivery_with_status(DeliveryStatus::Ready);
    let planning = completed_active_binding(
        advance(&ready, advance_input(1, "terminal-required"))
            .expect("planning starts")
            .delivery,
        "terminal-required",
    );
    let mut review = advance_input(2, "terminal-required-review");
    review.review = Some(ReviewAttentionSeed {
        title: "Review".into(),
        context: "frozen-terminal-review".into(),
        assigned_to: "reviewer-one".into(),
    });
    assert!(advance(&planning, review).is_err());

    let run = &planning.snapshot().stage_runs[0];
    let binding = &planning.snapshot().session_bindings[0];
    let worker_session_id = binding.worker_session_id.clone().expect("worker session");
    let lease = ActiveLeaseIdentity {
        execution_job_id: binding.execution_job_id.clone(),
        attempt: run.attempt,
        lease_id: LeaseId("lease-terminal-required".into()),
        fencing_token: FencingToken("7".into()),
        worker_id: WorkerId("worker-one".into()),
        worker_instance_id: WorkerInstanceId("worker-instance-one".into()),
        worker_session_id: worker_session_id.clone(),
    };
    let wrong_fencing = TerminalWorkerOutcome {
        stage_run_id: run.id.clone(),
        execution_job_id: binding.execution_job_id.clone(),
        attempt: run.attempt,
        lease_id: lease.lease_id.clone(),
        fencing_token: FencingToken("6".into()),
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id,
        status: TerminalOutcomeStatus::Succeeded,
    };
    let error = verify_terminal_outcome(&planning, &lease, wrong_fencing)
        .expect_err("stale fencing token must fail closed");
    assert_eq!(error.code(), CoordinationErrorCode::BindingConflict);
    assert_eq!(
        planning.snapshot().stage_runs[0].status,
        winwincode_delivery::domain::StageRunStatus::Running
    );
}
