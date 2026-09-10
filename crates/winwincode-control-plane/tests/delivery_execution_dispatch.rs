use std::sync::{Arc, Mutex};

use serde_json::Value;
use winwincode_control_plane::delivery_execution::{
    DeliveryExecutionCommitReceipt, DeliveryExecutionConfig, DeliveryExecutionPortError,
    DeliveryExecutionTransaction, ExecutionJobDispatcher, PendingDeliveryExecution,
    acknowledge_job_cancel, commit_and_dispatch, prepare_workrun_advance,
};
use winwincode_delivery::{
    application::stage::{
        ExecutionIntent, NewStageIdentities, StageAdvanceEffect, StageAdvanceResult,
        request_cancel, test_support::active_lease_identity,
    },
    domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStatus, DeliveryTask, DeliveryTaskStatus,
        SessionBindingId,
    },
};
use winwincode_domain::{
    AttentionItemId, DeliveryId, DeliveryTaskId, ExecutionJobId, ExecutionMessageId, Instant,
    ProductSessionId, RepositoryId, RequestId, Revision, SchemaVersion, SessionIdentity,
    Sha256Digest, StageRunId, WorkContractId, WorkItemId, WorkRunId,
};
use winwincode_execution_port::generated::{
    ExecutionLeaseStamp, ExecutionLimits, ExecutionScope, ExecutionWorkspace, JobCancelAckMessage,
    JobCancelAckMessageKind, JobCancelAckMessageStatus, WorkRunExecutionScope,
};

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn workrun_dispatch(seed: u64, with_task: bool) -> StageAdvanceResult {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical fixture")
    .into_snapshot();
    let delivery_id = DeliveryId(canonical_id("dlv", seed));
    let work_contract_id = WorkContractId(canonical_id("wct", seed));
    snapshot.work_run_aggregate.contract.id = work_contract_id.clone();
    snapshot.work_run_aggregate.contract.revision = Revision(1);
    snapshot.work_run_aggregate.items.truncate(1);
    snapshot.work_run_aggregate.items[0].id = WorkItemId(canonical_id("wit", seed));
    snapshot.work_run_aggregate.items[0].work_contract_id = work_contract_id.clone();
    snapshot.work_run_aggregate.items[0].work_contract_revision = Revision(1);
    snapshot.work_run_aggregate.items[0].revision = Revision(1);
    snapshot.work_run_aggregate.items[0].state = winwincode_domain::WorkItemState::Ready;
    snapshot.work_run_aggregate.items[0].depends_on.clear();
    snapshot.work_run_aggregate.runs.clear();
    snapshot.id = delivery_id.clone();
    snapshot.spec.delivery_id = delivery_id.clone();
    snapshot.revision = 1;
    snapshot.status = if with_task {
        DeliveryStatus::Executing
    } else {
        DeliveryStatus::Draft
    };
    snapshot.tasks.clear();
    if with_task {
        snapshot.tasks.push(DeliveryTask {
            schema_version: DELIVERY_SCHEMA_VERSION,
            id: DeliveryTaskId(canonical_id("dtk", seed)),
            delivery_id,
            title: "Implement the approved task".into(),
            goal: "Implement the approved candidate change.".into(),
            acceptance_criterion_ids: vec![snapshot.spec.acceptance_criteria[0].id.clone()],
            blocked_by_task_ids: Vec::new(),
            owner: None,
            status: DeliveryTaskStatus::Pending,
        });
    }
    snapshot.stage_runs.clear();
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.updated_at_millis = snapshot.created_at_millis;
    let delivery = Delivery::try_from_snapshot(snapshot).expect("draft Delivery");
    let selected = delivery
        .snapshot()
        .work_run_aggregate
        .start_next()
        .expect("ready WorkItem");
    StageAdvanceResult::canonical_workrun_dispatch(
        &delivery,
        None,
        NewStageIdentities {
            stage_run_id: StageRunId(canonical_id("run", seed)),
            work_contract_id,
            work_contract_revision: Revision(1),
            work_item_id: selected.work_item.id.clone(),
            work_item_revision: selected.work_item.revision.clone(),
            work_run_id: WorkRunId(canonical_id("wrn", seed)),
            execution_job_id: ExecutionJobId(canonical_id("job", seed)),
            session_binding_id: SessionBindingId::new(format!("binding-{seed}"))
                .expect("binding id"),
            attention_item_id: AttentionItemId(canonical_id("att", seed)),
        },
        ProductSessionId(canonical_id("psn", seed)),
        "executor".into(),
        selected.work_item.goal.clone(),
        u64::try_from(selected.attempt).expect("positive attempt"),
        1_800_000_000_100,
    )
    .expect("canonical WorkRun dispatch")
}

fn execution_config(seed: u64) -> DeliveryExecutionConfig {
    DeliveryExecutionConfig {
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        candidate_ref: None,
        workspace: ExecutionWorkspace {
            checkout_revision: "0123456789abcdef".into(),
            repository_id: RepositoryId(canonical_id("rep", seed)),
            write_mode:
                winwincode_execution_port::generated::ExecutionWorkspaceWriteMode::Candidate,
        },
        limits: ExecutionLimits {
            deadline_at: Instant("2026-08-25T12:00:00.000Z".into()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
    }
}

fn pending_execution(seed: u64, with_task: bool) -> PendingDeliveryExecution {
    let config = execution_config(seed);
    let request_id = RequestId(canonical_id("req", seed));
    let transition = workrun_dispatch(seed, with_task);
    let StageAdvanceEffect::Dispatch(intent) = &transition.effect else {
        panic!("stage advance must create a dispatch intent");
    };
    let job = prepare_workrun_advance(
        &request_id,
        &transition.delivery.snapshot().work_run_aggregate,
        &transition.delivery.snapshot().spec,
        intent,
        config,
    )
    .expect("prepared execution job");
    let input = job.work_input.as_ref().expect("execution input");
    assert_eq!(
        input.delivery_spec_id,
        transition.delivery.snapshot().spec.id.0
    );
    assert_eq!(
        u64::try_from(input.delivery_spec_revision.0).expect("positive spec revision"),
        transition.delivery.snapshot().spec.revision
    );
    PendingDeliveryExecution::from_workrun(request_id, transition, job)
}

fn dispatch_intent_mut(result: &mut StageAdvanceResult) -> &mut ExecutionIntent {
    let StageAdvanceEffect::Dispatch(intent) = &mut result.effect else {
        panic!("test advance must create a dispatch intent");
    };
    intent
}

fn assert_prepare_rejected(
    name: &str,
    request_id: &RequestId,
    result: &StageAdvanceResult,
    config: DeliveryExecutionConfig,
) {
    let (StageAdvanceEffect::Dispatch(intent) | StageAdvanceEffect::Resume(intent)) =
        &result.effect
    else {
        panic!("malformed fixture must carry execution intent");
    };
    let error = prepare_workrun_advance(
        request_id,
        &result.delivery.snapshot().work_run_aggregate,
        &result.delivery.snapshot().spec,
        intent,
        config,
    )
    .expect_err("malformed value must fail before pending publication");
    assert!(
        matches!(
            &error,
            winwincode_control_plane::delivery_execution::DeliveryExecutionError::InvalidEffect(_)
                | winwincode_control_plane::delivery_execution::DeliveryExecutionError::Coordination(_)
        ),
        "{name}: {error}"
    );
}

struct RecordingTransaction {
    trace: Arc<Mutex<Vec<String>>>,
    replayed: bool,
    acknowledge_error: bool,
}

impl DeliveryExecutionTransaction for RecordingTransaction {
    fn commit_delivery_and_job_intent(
        &mut self,
        pending: &PendingDeliveryExecution,
    ) -> Result<DeliveryExecutionCommitReceipt, DeliveryExecutionPortError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push(format!("commit:{}", pending.job().job_id.0));
        Ok(DeliveryExecutionCommitReceipt {
            committed_revision: pending.delivery().revision(),
            outbox_event_id: format!("outbox:{}", pending.job().job_id.0),
            job: pending.job().clone(),
            replayed: self.replayed,
        })
    }

    fn mark_job_dispatched(
        &mut self,
        outbox_event_id: &str,
    ) -> Result<(), DeliveryExecutionPortError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push(format!("ack:{outbox_event_id}"));
        if self.acknowledge_error {
            Err(DeliveryExecutionPortError::new("outbox ack failed"))
        } else {
            Ok(())
        }
    }
}

struct RecordingDispatcher {
    trace: Arc<Mutex<Vec<String>>>,
}

impl ExecutionJobDispatcher for RecordingDispatcher {
    fn dispatch(
        &mut self,
        job: &winwincode_execution_port::generated::ExecutionJob,
    ) -> Result<(), DeliveryExecutionPortError> {
        self.trace
            .lock()
            .expect("trace lock")
            .push(format!("dispatch:{}", job.job_id.0));
        Ok(())
    }
}

#[test]
fn delivery_advance_dispatches_one_execution_job_after_commit() {
    let pending = pending_execution(1, false);
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut transaction = RecordingTransaction {
        trace: Arc::clone(&trace),
        replayed: false,
        acknowledge_error: false,
    };
    let mut dispatcher = RecordingDispatcher {
        trace: Arc::clone(&trace),
    };

    let receipt = commit_and_dispatch(&pending, &mut transaction, &mut dispatcher)
        .expect("commit then dispatch");

    assert!(receipt.dispatched);
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            format!("commit:{}", canonical_id("job", 1)),
            format!("dispatch:{}", canonical_id("job", 1)),
            format!("ack:outbox:{}", canonical_id("job", 1)),
        ]
    );
}

#[test]
fn failed_outbox_ack_keeps_the_dispatched_job_durably_pending() {
    let pending = pending_execution(12, false);
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut transaction = RecordingTransaction {
        trace: Arc::clone(&trace),
        replayed: false,
        acknowledge_error: true,
    };
    let mut dispatcher = RecordingDispatcher {
        trace: Arc::clone(&trace),
    };

    let error = commit_and_dispatch(&pending, &mut transaction, &mut dispatcher)
        .expect_err("dispatch acknowledgement must remain replayable");

    assert_eq!(
        error
            .committed_receipt()
            .expect("committed receipt")
            .outbox_event_id,
        format!("outbox:{}", canonical_id("job", 12))
    );
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [
            format!("commit:{}", canonical_id("job", 12)),
            format!("dispatch:{}", canonical_id("job", 12)),
            format!("ack:outbox:{}", canonical_id("job", 12)),
        ]
    );
}

#[test]
fn replayed_delivery_advance_does_not_dispatch_a_second_execution_job() {
    let pending = pending_execution(2, false);
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut transaction = RecordingTransaction {
        trace: Arc::clone(&trace),
        replayed: true,
        acknowledge_error: false,
    };
    let mut dispatcher = RecordingDispatcher {
        trace: Arc::clone(&trace),
    };

    let receipt =
        commit_and_dispatch(&pending, &mut transaction, &mut dispatcher).expect("replay receipt");

    assert!(!receipt.dispatched);
    assert_eq!(receipt.commit.job, *pending.job());
    assert_eq!(
        receipt.commit.outbox_event_id,
        format!("outbox:{}", canonical_id("job", 2))
    );
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [format!("commit:{}", canonical_id("job", 2))]
    );
}

struct DurableReceiptTransaction {
    receipt: Option<DeliveryExecutionCommitReceipt>,
}

impl DeliveryExecutionTransaction for DurableReceiptTransaction {
    fn commit_delivery_and_job_intent(
        &mut self,
        _pending: &PendingDeliveryExecution,
    ) -> Result<DeliveryExecutionCommitReceipt, DeliveryExecutionPortError> {
        self.receipt
            .take()
            .ok_or_else(|| DeliveryExecutionPortError::new("receipt already consumed"))
    }

    fn mark_job_dispatched(
        &mut self,
        _outbox_event_id: &str,
    ) -> Result<(), DeliveryExecutionPortError> {
        Ok(())
    }
}

#[test]
fn new_commit_rejects_a_foreign_durable_job_without_dispatch() {
    let pending = pending_execution(8, false);
    let durable_job = pending_execution(9, false).job().clone();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut transaction = DurableReceiptTransaction {
        receipt: Some(DeliveryExecutionCommitReceipt {
            committed_revision: pending.delivery().revision(),
            outbox_event_id: "delivery-job-event-8".into(),
            job: durable_job,
            replayed: false,
        }),
    };
    let mut dispatcher = RecordingDispatcher {
        trace: Arc::clone(&trace),
    };

    let error = commit_and_dispatch(&pending, &mut transaction, &mut dispatcher)
        .expect_err("foreign durable job must stay committed and undispatched");

    assert_eq!(
        error
            .committed_receipt()
            .expect("committed receipt")
            .outbox_event_id,
        "delivery-job-event-8"
    );
    assert!(trace.lock().expect("trace lock").is_empty());
}

#[test]
fn corrupted_durable_receipt_job_stays_committed_and_is_not_dispatched() {
    let pending = pending_execution(10, false);
    let mut corrupted_job = pending.job().clone();
    corrupted_job.workspace.checkout_revision.clear();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut transaction = DurableReceiptTransaction {
        receipt: Some(DeliveryExecutionCommitReceipt {
            committed_revision: pending.delivery().revision(),
            outbox_event_id: "delivery-job-event-10".into(),
            job: corrupted_job,
            replayed: false,
        }),
    };
    let mut dispatcher = RecordingDispatcher {
        trace: Arc::clone(&trace),
    };

    let error = commit_and_dispatch(&pending, &mut transaction, &mut dispatcher)
        .expect_err("corrupted durable job must stay pending after commit");

    assert_eq!(
        error
            .committed_receipt()
            .expect("committed receipt")
            .outbox_event_id,
        "delivery-job-event-10"
    );
    assert!(trace.lock().expect("trace lock").is_empty());
}

#[test]
fn workrun_scope_carries_exact_contract_item_and_run_identity() {
    let pending = pending_execution(3, true);
    let ExecutionScope::WorkRunExecutionScope(WorkRunExecutionScope {
        kind,
        attempt,
        work_contract_id,
        work_contract_revision,
        work_item_id,
        work_item_revision,
        product_session_id,
        rework_authorization,
        work_run_id,
    }) = &pending.job().scope
    else {
        panic!("Delivery stage dispatch must use the Delivery scope");
    };

    assert_eq!(
        kind,
        &winwincode_execution_port::generated::WorkRunExecutionScopeKind::WorkRun
    );
    assert_eq!(*attempt, 1);
    assert_eq!(work_contract_id.0, canonical_id("wct", 3));
    assert_eq!(*work_contract_revision, Revision(1));
    assert_eq!(work_item_id.0, canonical_id("wit", 3));
    assert_eq!(*work_item_revision, Revision(1));
    assert_eq!(product_session_id.0, canonical_id("psn", 3));
    assert_eq!(work_run_id.0, canonical_id("wrn", 3));
    assert!(rework_authorization.is_none());
    assert_eq!(pending.job().execution_profile, "executor");
}

#[test]
fn job_cancel_ack_does_not_settle_workrun_before_terminal_outcome() {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("accepted execution fixture")
    .into_snapshot();
    snapshot.stage_runs.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    snapshot.status = DeliveryStatus::Executing;
    snapshot.work_run_aggregate.runs[0].state = winwincode_domain::WorkRunState::Running;
    snapshot.work_run_aggregate.items[0].state = winwincode_domain::WorkItemState::InProgress;
    let run_id = snapshot.work_run_aggregate.runs[0].id.clone();
    let delivery = Delivery::try_from_snapshot(snapshot).expect("accepted WorkRun");
    let binding = &delivery.snapshot().session_bindings[0];
    let intent = request_cancel(&delivery, delivery.revision(), &run_id).expect("cancel intent");
    assert_eq!(binding.execution_job_id, intent.execution_job_id);
    let lease = active_lease_identity(
        intent.execution_job_id.clone(),
        intent.attempt,
        binding
            .lease_id
            .clone()
            .expect("cancel acknowledgement lease"),
        binding
            .fencing_token
            .clone()
            .expect("cancel acknowledgement fence"),
        binding
            .worker_id
            .clone()
            .expect("cancel acknowledgement Worker"),
        binding
            .worker_instance_id
            .clone()
            .expect("cancel acknowledgement Worker instance"),
        intent.worker_session_id.clone(),
    );
    let session_identity = SessionIdentity {
        codex_thread_id: binding
            .codex_thread_id
            .clone()
            .expect("cancel acknowledgement CodexThread"),
        product_session_id: binding.product_session_id.clone(),
        work_run_id: Some(run_id.clone()),
        worker_session_id: lease.worker_session_id().clone(),
    };
    let ack = JobCancelAckMessage {
        error: None,
        kind: JobCancelAckMessageKind::JobCancelAck,
        lease: ExecutionLeaseStamp {
            attempt: i64::try_from(lease.attempt()).expect("attempt"),
            expires_at: Instant("2026-08-25T12:10:00.000Z".into()),
            fencing_token: lease.fencing_token().clone(),
            issued_at: Instant("2026-08-25T12:00:00.000Z".into()),
            job_id: lease.execution_job_id().clone(),
            lease_id: lease.lease_id().clone(),
            worker_id: lease.worker_id().clone(),
            worker_instance_id: lease.worker_instance_id().clone(),
        },
        message_id: ExecutionMessageId(canonical_id("xmsg", 4)),
        request_id: RequestId(canonical_id("req", 4)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2026-08-25T12:00:01.000Z".into()),
        session_identity,
        status: JobCancelAckMessageStatus::Accepted,
        worker_session_id: lease.worker_session_id().clone(),
    };

    let after_ack = acknowledge_job_cancel(
        &delivery,
        &intent,
        &lease,
        &RequestId(canonical_id("req", 4)),
        &ack,
    )
    .expect("exact generated acknowledgement");

    assert_eq!(after_ack, delivery);
    assert_eq!(
        after_ack.snapshot().work_run_aggregate.runs[0].state,
        winwincode_domain::WorkRunState::Running
    );
    assert_eq!(
        after_ack.snapshot().work_run_aggregate.items[0].state,
        winwincode_domain::WorkItemState::InProgress
    );
    assert!(after_ack.snapshot().stage_runs.is_empty());
}

#[test]
fn delivery_dispatch_does_not_persist_codex_plan_agent_or_tool_state() {
    let pending = pending_execution(5, true);
    let serialized = serde_json::to_value(pending.job()).expect("serialize generated job");
    let object = serialized.as_object().expect("ExecutionJob object");

    for forbidden in [
        "codexPlan",
        "plan",
        "agentGraph",
        "agents",
        "toolCall",
        "toolCalls",
        "schedulerState",
    ] {
        assert!(
            !object.contains_key(forbidden),
            "forbidden key: {forbidden}"
        );
    }
    assert_eq!(object.len(), 9);
    let work_input = object["workInput"]
        .as_object()
        .expect("typed WorkContract and WorkItem");
    assert_eq!(work_input["workContract"]["id"], canonical_id("wct", 5));
    assert_eq!(work_input["workContract"]["revision"], 1);
    assert_eq!(
        work_input["workContract"]["criteria"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(work_input["workItem"]["id"], canonical_id("wit", 5));
    assert!(work_input.get("candidateRef").is_none());
    assert!(pending.delivery().snapshot().session_bindings.is_empty());
    assert!(
        pending
            .delivery()
            .snapshot()
            .work_run_aggregate
            .runs
            .is_empty()
    );
}

#[test]
fn prepared_execution_job_serializes_as_schema_valid_fixture() {
    let pending = pending_execution(6, true);
    let actual = serde_json::to_value(pending.job()).expect("serialize generated job");
    let expected: Value = serde_json::from_slice(include_bytes!(
        "fixtures/prepared-delivery-execution-job.json"
    ))
    .expect("schema-validated fixture");

    assert_eq!(actual, expected);
}

type ConfigMutation = fn(&mut DeliveryExecutionConfig);
type IntentMutation = fn(&mut ExecutionIntent);

#[test]
fn malformed_execution_job_config_fails_before_pending_publication() {
    let seed = 11;
    let request_id = RequestId(canonical_id("req", seed));

    assert_prepare_rejected(
        "requestId",
        &RequestId("request-legacy".into()),
        &workrun_dispatch(seed, false),
        execution_config(seed),
    );
    let mut config = execution_config(seed);
    config.payload_digest = Sha256Digest("a".repeat(64));
    assert_prepare_rejected(
        "payloadDigest",
        &request_id,
        &workrun_dispatch(seed, false),
        config,
    );
    assert_invalid_config_values(seed, &request_id);
    assert_invalid_intent_values(seed, &request_id);
}

fn assert_invalid_config_values(seed: u64, request_id: &RequestId) {
    let config_mutations: [(&str, ConfigMutation); 5] = [
        ("repositoryId", |config| {
            config.workspace.repository_id = RepositoryId("repository-legacy".into());
        }),
        ("checkoutRevision", |config| {
            config.workspace.checkout_revision.clear();
        }),
        ("deadlineAt", |config| {
            config.limits.deadline_at = Instant("2026-08-25T12:00:00Z".into());
        }),
        ("maxRuntimeSeconds", |config| {
            config.limits.max_runtime_seconds = 604_801;
        }),
        ("maxArtifactBytes", |config| {
            config.limits.max_artifact_bytes = 1_099_511_627_777;
        }),
    ];
    for (name, mutate) in config_mutations {
        let mut config = execution_config(seed);
        mutate(&mut config);
        assert_prepare_rejected(name, request_id, &workrun_dispatch(seed, false), config);
    }

    let mut long_checkout = execution_config(seed);
    long_checkout.workspace.checkout_revision = "r".repeat(201);
    assert_prepare_rejected(
        "checkoutRevision maxLength",
        request_id,
        &workrun_dispatch(seed, false),
        long_checkout,
    );
    let mut zero_limits = execution_config(seed);
    zero_limits.limits.max_runtime_seconds = 0;
    zero_limits.limits.max_artifact_bytes = -1;
    assert_prepare_rejected(
        "limit minima",
        request_id,
        &workrun_dispatch(seed, false),
        zero_limits,
    );
}

fn assert_invalid_intent_values(seed: u64, request_id: &RequestId) {
    let intent_mutations: [(&str, IntentMutation); 7] = [
        ("jobId", |intent| {
            intent.execution_job_id = ExecutionJobId("job-legacy".into());
        }),
        ("productSessionId", |intent| {
            intent.product_session_id = ProductSessionId("product-session-legacy".into());
        }),
        ("workContractId", |intent| {
            intent.work_contract_id = WorkContractId("wct_1D2305PEEPBFFSFS4N091XCRX6".into());
        }),
        ("workRunId", |intent| {
            intent.work_run_id = WorkRunId("wrn_legacy".into());
        }),
        ("attempt", |intent| intent.attempt = 1_001),
        ("executionProfile", |intent| intent.role.clear()),
        ("goal", |intent| intent.goal.clear()),
    ];
    for (name, mutate) in intent_mutations {
        let mut result = workrun_dispatch(seed, false);
        mutate(dispatch_intent_mut(&mut result));
        assert_prepare_rejected(name, request_id, &result, execution_config(seed));
    }

    let mut task_result = workrun_dispatch(seed, true);
    dispatch_intent_mut(&mut task_result).work_item_id = WorkItemId("task-legacy".into());
    let mut writer_config = execution_config(seed);
    writer_config.workspace.write_mode =
        winwincode_execution_port::generated::ExecutionWorkspaceWriteMode::Candidate;
    assert_prepare_rejected("deliveryTaskId", request_id, &task_result, writer_config);
    let mut long_profile = workrun_dispatch(seed, false);
    dispatch_intent_mut(&mut long_profile).role = "r".repeat(101);
    assert_prepare_rejected(
        "executionProfile maxLength",
        request_id,
        &long_profile,
        execution_config(seed),
    );
    let mut long_goal = workrun_dispatch(seed, false);
    dispatch_intent_mut(&mut long_goal).goal = "g".repeat(20_001);
    assert_prepare_rejected(
        "goal maxLength",
        request_id,
        &long_goal,
        execution_config(seed),
    );
}
