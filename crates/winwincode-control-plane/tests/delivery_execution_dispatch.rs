use std::sync::{Arc, Mutex};

use serde_json::Value;
use winwincode_api::generated::{
    DeliveryStageExecutionScope, ExecutionLeaseStamp, ExecutionLimits, ExecutionScope,
    ExecutionWorkspace, JobCancelAckMessage, SchemaVersion,
};
use winwincode_control_plane::delivery_execution::{
    DeliveryExecutionCommitReceipt, DeliveryExecutionConfig, DeliveryExecutionPortError,
    DeliveryExecutionTransaction, ExecutionJobDispatcher, PendingDeliveryExecution,
    acknowledge_job_cancel, commit_and_dispatch, prepare_delivery_advance,
};
use winwincode_delivery::{
    application::stage::{
        ActiveLeaseIdentity, AdvanceStageInput, ExecutionIntent, NewStageIdentities,
        StageAdvanceEffect, StageAdvanceResult, advance, request_cancel,
    },
    domain::{
        DELIVERY_SCHEMA_VERSION, Delivery, DeliveryStatus, DeliveryTask, DeliveryTaskStatus,
        SessionBindingId,
    },
};
use winwincode_domain::{
    AttentionItemId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId, ExecutionMessageId,
    FencingToken, Instant, LeaseId, ProductSessionId, RepositoryId, RequestId, Sha256Digest,
    StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn stage_advance(seed: u64, with_task: bool) -> StageAdvanceResult {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical fixture")
    .into_snapshot();
    let delivery_id = DeliveryId(canonical_id("dlv", seed));
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
    advance(
        &delivery,
        AdvanceStageInput {
            expected_revision: 1,
            product_session_id: ProductSessionId(canonical_id("psn", seed)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(canonical_id("run", seed)),
                execution_job_id: ExecutionJobId(canonical_id("job", seed)),
                session_binding_id: SessionBindingId::new(format!("binding-{seed}"))
                    .expect("binding id"),
                attention_item_id: AttentionItemId(canonical_id("att", seed)),
            },
            review: None,
            previous_outcome: None,
            current_lease: None,
            now_millis: 1_800_000_000_100,
        },
    )
    .expect("stage advance")
}

fn execution_config(seed: u64) -> DeliveryExecutionConfig {
    DeliveryExecutionConfig {
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        workspace: ExecutionWorkspace {
            checkout_revision: "0123456789abcdef".into(),
            repository_id: RepositoryId(canonical_id("rep", seed)),
            write_mode: "candidate".into(),
        },
        limits: ExecutionLimits {
            deadline_at: Instant("2026-08-25T12:00:00.000Z".into()),
            max_artifact_bytes: 10_000_000,
            max_runtime_seconds: 3_600,
        },
    }
}

fn pending_execution(seed: u64, with_task: bool) -> PendingDeliveryExecution {
    prepare_delivery_advance(
        RequestId(canonical_id("req", seed)),
        stage_advance(seed, with_task),
        execution_config(seed),
    )
    .expect("pending execution")
}

fn dispatch_intent_mut(result: &mut StageAdvanceResult) -> &mut ExecutionIntent {
    let StageAdvanceEffect::Dispatch(intent) = &mut result.effect else {
        panic!("test advance must create a dispatch intent");
    };
    intent
}

fn assert_prepare_rejected(
    name: &str,
    request_id: RequestId,
    result: StageAdvanceResult,
    config: DeliveryExecutionConfig,
) {
    let error = prepare_delivery_advance(request_id, result, config)
        .expect_err("malformed value must fail before pending publication");
    assert!(error.to_string().contains("invalid"), "{name}: {error}");
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
        job: &winwincode_api::generated::ExecutionJob,
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
    corrupted_job.workspace.write_mode = "worktree".into();
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
fn delivery_stage_scope_carries_exact_product_delivery_task_and_run_identity() {
    let pending = pending_execution(3, true);
    let ExecutionScope::DeliveryStageExecutionScope(DeliveryStageExecutionScope {
        delivery_id,
        delivery_task_id,
        kind,
        product_session_id,
        stage_run_id,
    }) = &pending.job().scope
    else {
        panic!("Delivery stage dispatch must use the Delivery scope");
    };

    assert_eq!(kind, "delivery-stage");
    assert_eq!(delivery_id.0, canonical_id("dlv", 3));
    assert_eq!(
        delivery_task_id.as_ref().map(|id| id.0.as_str()),
        Some(canonical_id("dtk", 3).as_str())
    );
    assert_eq!(product_session_id.0, canonical_id("psn", 3));
    assert_eq!(stage_run_id.0, canonical_id("run", 3));
    assert_eq!(pending.job().execution_profile, "executor");
}

#[test]
fn job_cancel_ack_does_not_settle_stage_before_terminal_outcome() {
    let pending = pending_execution(4, false);
    let mut snapshot = pending.delivery().clone().into_snapshot();
    let binding = snapshot.session_bindings.last_mut().expect("binding");
    binding.worker_session_id = Some(WorkerSessionId(canonical_id("wsn", 4)));
    binding.codex_thread_id = Some(CodexThreadId(canonical_id("cdx", 4)));
    let delivery = Delivery::try_from_snapshot(snapshot).expect("accepted WorkerSession");
    let intent = request_cancel(&delivery, delivery.revision()).expect("cancel intent");
    let lease = ActiveLeaseIdentity {
        execution_job_id: intent.execution_job_id.clone(),
        attempt: intent.attempt,
        lease_id: LeaseId(canonical_id("lse", 4)),
        fencing_token: FencingToken("4".into()),
        worker_id: WorkerId(canonical_id("wrk", 4)),
        worker_instance_id: WorkerInstanceId(canonical_id("wki", 4)),
        worker_session_id: intent.worker_session_id.clone(),
    };
    let ack = JobCancelAckMessage {
        error: None,
        kind: "job.cancel_ack".into(),
        lease: ExecutionLeaseStamp {
            attempt: i64::try_from(lease.attempt).expect("attempt"),
            expires_at: Instant("2026-08-25T12:10:00.000Z".into()),
            fencing_token: lease.fencing_token.clone(),
            issued_at: Instant("2026-08-25T12:00:00.000Z".into()),
            job_id: lease.execution_job_id.clone(),
            lease_id: lease.lease_id.clone(),
            worker_id: lease.worker_id.clone(),
            worker_instance_id: lease.worker_instance_id.clone(),
        },
        message_id: ExecutionMessageId(canonical_id("xmsg", 4)),
        request_id: RequestId(canonical_id("req", 4)),
        schema_version: SchemaVersion::WinwincodeV1,
        sent_at: Instant("2026-08-25T12:00:01.000Z".into()),
        status: "accepted".into(),
        worker_session_id: lease.worker_session_id.clone(),
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
        after_ack.snapshot().stage_runs[0].status,
        winwincode_delivery::domain::StageRunStatus::Running
    );
    assert!(
        after_ack.snapshot().stage_runs[0]
            .finished_at_millis
            .is_none()
    );
    // Retain the pending value to prove an acknowledgement does not consume
    // or replace its immutable job intent either.
    assert_eq!(pending.job().job_id, intent.execution_job_id);
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
    assert_eq!(object.len(), 8);
    assert_eq!(pending.delivery().snapshot().session_bindings.len(), 1);
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
        RequestId("request-legacy".into()),
        stage_advance(seed, false),
        execution_config(seed),
    );
    let mut config = execution_config(seed);
    config.payload_digest = Sha256Digest("a".repeat(64));
    assert_prepare_rejected(
        "payloadDigest",
        request_id.clone(),
        stage_advance(seed, false),
        config,
    );
    assert_invalid_config_values(seed, &request_id);
    assert_invalid_intent_values(seed, &request_id);
}

fn assert_invalid_config_values(seed: u64, request_id: &RequestId) {
    let config_mutations: [(&str, ConfigMutation); 6] = [
        ("repositoryId", |config| {
            config.workspace.repository_id = RepositoryId("repository-legacy".into());
        }),
        ("checkoutRevision", |config| {
            config.workspace.checkout_revision.clear();
        }),
        ("writeMode", |config| {
            config.workspace.write_mode = "worktree".into();
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
        assert_prepare_rejected(name, request_id.clone(), stage_advance(seed, false), config);
    }

    let mut long_checkout = execution_config(seed);
    long_checkout.workspace.checkout_revision = "r".repeat(201);
    assert_prepare_rejected(
        "checkoutRevision maxLength",
        request_id.clone(),
        stage_advance(seed, false),
        long_checkout,
    );
    let mut zero_limits = execution_config(seed);
    zero_limits.limits.max_runtime_seconds = 0;
    zero_limits.limits.max_artifact_bytes = -1;
    assert_prepare_rejected(
        "limit minima",
        request_id.clone(),
        stage_advance(seed, false),
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
        ("deliveryId", |intent| {
            intent.delivery_id = DeliveryId("delivery-legacy".into());
        }),
        ("stageRunId", |intent| {
            intent.stage_run_id = StageRunId("stage-legacy".into());
        }),
        ("attempt", |intent| intent.attempt = 1_001),
        ("executionProfile", |intent| intent.role.clear()),
        ("goal", |intent| intent.goal.clear()),
    ];
    for (name, mutate) in intent_mutations {
        let mut result = stage_advance(seed, false);
        mutate(dispatch_intent_mut(&mut result));
        assert_prepare_rejected(name, request_id.clone(), result, execution_config(seed));
    }

    let mut task_result = stage_advance(seed, true);
    dispatch_intent_mut(&mut task_result).delivery_task_id =
        Some(DeliveryTaskId("task-legacy".into()));
    assert_prepare_rejected(
        "deliveryTaskId",
        request_id.clone(),
        task_result,
        execution_config(seed),
    );
    let mut long_profile = stage_advance(seed, false);
    dispatch_intent_mut(&mut long_profile).role = "r".repeat(101);
    assert_prepare_rejected(
        "executionProfile maxLength",
        request_id.clone(),
        long_profile,
        execution_config(seed),
    );
    let mut long_goal = stage_advance(seed, false);
    dispatch_intent_mut(&mut long_goal).goal = "g".repeat(20_001);
    assert_prepare_rejected(
        "goal maxLength",
        request_id.clone(),
        long_goal,
        execution_config(seed),
    );
}
