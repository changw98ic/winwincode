use std::sync::{Arc, Mutex};

use serde_json::Value;
use winwincode_api::generated::{
    DeliveryStageExecutionScope, ExecutionLeaseStamp, ExecutionLimits, ExecutionScope,
    ExecutionWorkspace, JobCancelAckMessage, SchemaVersion,
};
use winwincode_control_plane::delivery_execution::{
    acknowledge_job_cancel, commit_and_dispatch, prepare_delivery_advance,
    DeliveryExecutionCommitReceipt, DeliveryExecutionConfig, DeliveryExecutionPortError,
    DeliveryExecutionTransaction, ExecutionJobDispatcher, PendingDeliveryExecution,
};
use winwincode_delivery::{
    application::stage::{
        advance, request_cancel, ActiveLeaseIdentity, AdvanceStageInput, NewStageIdentities,
    },
    domain::{
        Delivery, DeliveryStatus, DeliveryTask, DeliveryTaskStatus, SessionBindingId,
        DELIVERY_SCHEMA_VERSION,
    },
};
use winwincode_domain::{
    AttentionItemId, CodexThreadId, DeliveryId, DeliveryTaskId, ExecutionJobId,
    ExecutionMessageId, FencingToken, Instant, LeaseId, ProductSessionId, RepositoryId, RequestId,
    Sha256Digest, StageRunId, WorkerId, WorkerInstanceId, WorkerSessionId,
};

fn canonical_id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn pending_execution(seed: u64, with_task: bool) -> PendingDeliveryExecution {
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
            acceptance_criterion_ids: vec![
                snapshot.spec.acceptance_criteria[0].id.clone(),
            ],
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
    let result = advance(
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
            now_millis: 1_800_000_000_100,
        },
    )
    .expect("stage advance");
    prepare_delivery_advance(
        RequestId(canonical_id("req", seed)),
        result,
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
        },
    )
    .expect("pending execution")
}

struct RecordingTransaction {
    trace: Arc<Mutex<Vec<String>>>,
    replayed: bool,
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
            replayed: self.replayed,
        })
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
    };
    let mut dispatcher = RecordingDispatcher {
        trace: Arc::clone(&trace),
    };

    let receipt = commit_and_dispatch(&pending, &mut transaction, &mut dispatcher)
        .expect("replay receipt");

    assert!(!receipt.dispatched);
    assert_eq!(
        *trace.lock().expect("trace lock"),
        [format!("commit:{}", canonical_id("job", 2))]
    );
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
    assert!(after_ack.snapshot().stage_runs[0]
        .finished_at_millis
        .is_none());
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
        assert!(!object.contains_key(forbidden), "forbidden key: {forbidden}");
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

#[test]
fn malformed_execution_job_config_fails_before_pending_publication() {
    let mut snapshot = Delivery::decode_json(include_bytes!(
        "../../winwincode-delivery/tests/fixtures/delivery-main.json"
    ))
    .expect("canonical fixture")
    .into_snapshot();
    snapshot.id = DeliveryId(canonical_id("dlv", 7));
    snapshot.spec.delivery_id = snapshot.id.clone();
    snapshot.revision = 1;
    snapshot.status = DeliveryStatus::Draft;
    snapshot.tasks.clear();
    snapshot.stage_runs.clear();
    snapshot.session_bindings.clear();
    snapshot.attention_items.clear();
    snapshot.evidence.clear();
    snapshot.verdict = None;
    let delivery = Delivery::try_from_snapshot(snapshot).expect("Delivery");
    let result = advance(
        &delivery,
        AdvanceStageInput {
            expected_revision: 1,
            product_session_id: ProductSessionId(canonical_id("psn", 7)),
            identities: NewStageIdentities {
                stage_run_id: StageRunId(canonical_id("run", 7)),
                execution_job_id: ExecutionJobId(canonical_id("job", 7)),
                session_binding_id: SessionBindingId::new("binding-7").expect("binding"),
                attention_item_id: AttentionItemId(canonical_id("att", 7)),
            },
            review: None,
            previous_outcome: None,
            now_millis: 1_800_000_000_100,
        },
    )
    .expect("advance");

    let error = prepare_delivery_advance(
        RequestId(canonical_id("req", 7)),
        result,
        DeliveryExecutionConfig {
            payload_digest: Sha256Digest("a".repeat(64)),
            workspace: ExecutionWorkspace {
                checkout_revision: "revision".into(),
                repository_id: RepositoryId(canonical_id("rep", 7)),
                write_mode: "worktree".into(),
            },
            limits: ExecutionLimits {
                deadline_at: Instant("2026-08-25T12:00:00Z".into()),
                max_artifact_bytes: 1_099_511_627_777,
                max_runtime_seconds: 604_801,
            },
        },
    )
    .expect_err("all malformed schema values must fail before publication");

    assert!(error.to_string().contains("invalid"));
}
