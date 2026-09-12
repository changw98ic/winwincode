// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "test-support")]
#![allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::drop_non_drop
)]
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use winwincode_delivery::domain::Delivery;
use winwincode_delivery::store::{
    AppendDeliveryWorkRun, CreateDelivery, DeliveryCommand, DeliveryCommandPort, DeliveryQuery,
    DeliveryQueryPort, DeliveryStoreErrorCode,
};
use winwincode_domain::{
    ExecutionJobId, ExecutionMessageId, FencingToken, Instant, LeaseId, RequestId, Sha256Digest,
    WorkerId, WorkerInstanceId,
};
use winwincode_storage::{
    DispatchResultRequest, DispatchResultStatus, EXECUTION_PROTOCOL_VERSION, ExecutionLeaseClaim,
    ProductStateStorage, SqliteStorage, WorkerAuthenticationIdentity, WorkerPlatform,
    WorkerRegistrationRequest,
};
static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn fixture() -> Delivery {
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .expect("execution-port fixture");
    let job = source["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["kind"] == "job.dispatch")
        .expect("job.dispatch");
    let input = &job["job"]["workInput"];
    let mut delivery: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/delivery-main.json"))
            .expect("delivery fixture");
    delivery["revision"] = 1.into();
    delivery["status"] = "draft".into();
    for field in ["sessionBindings", "evidence", "attentionItems"] {
        delivery[field] = serde_json::json!([]);
    }
    delivery["verdict"] = serde_json::Value::Null;
    delivery["workRunAggregate"]["contract"] = input["workContract"].clone();
    delivery["workRunAggregate"]["items"] = serde_json::json!([input["workItem"].clone()]);
    delivery["workRunAggregate"]["runs"] = serde_json::json!([]);
    Delivery::decode_json(&serde_json::to_vec(&delivery).expect("delivery bytes"))
        .expect("canonical delivery with execution job work input")
}

fn temporary_directory(name: &str) -> PathBuf {
    let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-execution-registry-{name}-{}-{suffix}",
        std::process::id()
    ))
}

fn id(prefix: &str, value: u64) -> String {
    format!("{prefix}_{value:026}")
}

fn instant(second: u64) -> Instant {
    Instant(format!("2027-01-15T08:00:{second:02}.000Z"))
}

fn registration(seed: u64, instance: u64, request: u64) -> WorkerRegistrationRequest {
    WorkerRegistrationRequest {
        authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
            control_plane_principal: "fixture-control-plane".into(),
        },
        protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
        platform: WorkerPlatform::Aarch64AppleDarwin,
        capabilities: vec!["codex".into(), "artifact".into()],
        capability_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        security_zone: "local".into(),
        max_slots: 4,
        message_id: ExecutionMessageId(id("xmsg", request)),
        request_id: RequestId(id("req", request)),
        sent_at: instant(1),
        started_at: instant(0),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
    }
}

fn claim(
    seed: u64,
    instance: u64,
    request: u64,
    attempt: u64,
    fence: u64,
    issued_second: u64,
    expires_second: u64,
) -> ExecutionLeaseClaim {
    ExecutionLeaseClaim {
        expires_at: instant(expires_second),
        fencing_token: FencingToken(fence.to_string()),
        issued_at: instant(issued_second),
        job_id: ExecutionJobId(id("job", seed)),
        lease_id: LeaseId(id("lse", request)),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
        request_id: RequestId(id("req", request)),
        worker_id: WorkerId(id("wrk", seed)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
        attempt,
    }
}

fn dispatch_result(
    lease: &ExecutionLeaseClaim,
    request: u64,
    checked_second: u64,
) -> DispatchResultRequest {
    DispatchResultRequest {
        checked_at: instant(checked_second),
        expires_at: lease.expires_at.clone(),
        fencing_token: lease.fencing_token.clone(),
        issued_at: lease.issued_at.clone(),
        job_id: lease.job_id.clone(),
        lease_id: lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: lease.payload_digest.clone(),
        request_id: RequestId(id("req", request)),
        sent_at: lease.issued_at.clone(),
        status: DispatchResultStatus::Accepted,
        attempt: lease.attempt,
        error: None,
        worker_id: lease.worker_id.clone(),
        worker_instance_id: lease.worker_instance_id.clone(),
        worker_session_id: Some(winwincode_domain::WorkerSessionId(id("wsn", request))),
    }
}

#[test]
fn queue_proof_appends_work_run_and_replays() {
    use winwincode_domain::{
        OrganizationId, ProductSessionId, ProjectId, RepositoryId, WorkRun, WorkRunId, WorkspaceId,
    };
    use winwincode_storage::{ExecutionJobSubmission, ExecutionQueueScope};
    let root = temporary_directory("work-run-proof");
    let mut storage = SqliteStorage::open(&root).unwrap();
    let lease = claim(20, 1, 21, 1, 7, 1, 50);
    let port_fixture: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let mut job = port_fixture["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["kind"] == "job.dispatch")
        .unwrap()["job"]
        .clone();
    job["jobId"] = lease.job_id.0.clone().into();
    job["payloadDigest"] = lease.payload_digest.0.clone().into();
    job["attempt"] = 1.into();
    let scope = &job["scope"];
    let delivery = fixture();
    let delivery_id = delivery.id().clone();
    let queue_scope = ExecutionQueueScope {
        organization_id: OrganizationId(id("org", 20)),
        workspace_id: WorkspaceId(id("wsp", 20)),
        project_id: ProjectId(id("prj", 20)),
        repository_id: RepositoryId(job["workspace"]["repositoryId"].as_str().unwrap().into()),
        product_session_id: ProductSessionId(scope["productSessionId"].as_str().unwrap().into()),
        delivery_id: Some(delivery_id.clone()),
    };
    storage
        .execution_queue()
        .unwrap()
        .submit(&ExecutionJobSubmission {
            scope: queue_scope,
            job_id: lease.job_id.clone(),
            request_id: RequestId(id("req", 90)),
            payload_digest: lease.payload_digest.clone(),
            dispatch_payload: serde_json::to_vec(&job).unwrap(),
            attempt: 1,
            dependencies: vec![],
            work_run_id: Some(WorkRunId(scope["workRunId"].as_str().unwrap().into())),
            submitted_at: instant(0),
        })
        .unwrap();
    let mut registry = storage.execution_registry().unwrap();
    registry.register_worker(&registration(20, 1, 20)).unwrap();
    registry.claim_execution_job(&lease).unwrap();
    registry
        .record_dispatch_result(&dispatch_result(&lease, 22, 2))
        .unwrap();
    let authority = registry
        .load_dispatch_authority(&lease.job_id)
        .unwrap()
        .unwrap();
    let proof = registry
        .load_dispatch_work_run_proof(&authority, &instant(3))
        .unwrap();
    let run: WorkRun = serde_json::from_value(serde_json::json!({
        "schemaVersion":"winwincode/v1", "id":scope["workRunId"],
        "workContractId":scope["workContractId"], "contractRevision":scope["workContractRevision"],
        "workItemId":scope["workItemId"], "workItemRevision":scope["workItemRevision"],
        "revision":1, "state":"leased", "executionJobId":lease.job_id, "attempt":1,
        "workerId":lease.worker_id, "workerInstanceId":lease.worker_instance_id,
        "workerSessionId":authority.worker_session_id(), "leaseId":lease.lease_id,
        "fencingToken":lease.fencing_token, "productSessionId":scope["productSessionId"],
        "codexThreadId":null, "candidateDigest":null
    }))
    .unwrap();
    proof.verify_work_run(&run, &delivery_id.0).unwrap();
    drop(registry);
    let journal = std::sync::Arc::new(winwincode_delivery::store::InMemoryDeliveryJournal::new());
    let store = winwincode_delivery::store::DeliveryStore::new(journal);
    store
        .execute(DeliveryCommand::SeedForTest(CreateDelivery {
            request_id: RequestId(id("req", 100)),
            request_digest: "b".repeat(64),
            snapshot: delivery.clone(),
        }))
        .unwrap();
    let command = AppendDeliveryWorkRun {
        delivery_id: delivery_id.clone(),
        request_id: RequestId(id("req", 101)),
        request_digest: "c".repeat(64),
        expected_revision: delivery.revision(),
        run: run.clone(),
        authority: winwincode_storage::delivery_dispatch_authority(&authority),
        execution_profile: proof.execution_profile().unwrap(),
        now_millis: delivery.snapshot().updated_at_millis + 1,
    };
    for state in [
        winwincode_domain::WorkItemState::Done,
        winwincode_domain::WorkItemState::Failed,
        winwincode_domain::WorkItemState::Cancelled,
    ] {
        let mut snapshot = delivery.clone().into_snapshot();
        snapshot.work_run_aggregate.items[0].state = state;
        let unavailable = Delivery::try_from_snapshot(snapshot).unwrap();
        let isolated = winwincode_delivery::store::DeliveryStore::new(std::sync::Arc::new(
            winwincode_delivery::store::InMemoryDeliveryJournal::new(),
        ));
        isolated
            .execute(DeliveryCommand::SeedForTest(CreateDelivery {
                request_id: RequestId(id("req", 100)),
                request_digest: "b".repeat(64),
                snapshot: unavailable.clone(),
            }))
            .unwrap();
        let before = isolated
            .query(DeliveryQuery::Get(delivery_id.clone()))
            .unwrap();
        assert!(
            isolated
                .execute(DeliveryCommand::AppendWorkRun(Box::new(command.clone())))
                .is_err(),
            "valid queue proof must not revive a terminal task"
        );
        let after = isolated
            .query(DeliveryQuery::Get(delivery_id.clone()))
            .unwrap();
        assert_eq!(after, unavailable);
        assert_eq!(after, before);
    }
    let mut wrong = command.clone();
    wrong.run.work_item_id.0 = id("wit", 99);
    assert!(
        store
            .execute(DeliveryCommand::AppendWorkRun(Box::new(wrong)))
            .is_err()
    );
    for (field, value) in [
        (
            "candidateDigest",
            serde_json::json!(format!("sha256:{}", "e".repeat(64))),
        ),
        ("codexThreadId", serde_json::json!(id("cdx", 99))),
        ("state", serde_json::json!("running")),
        ("revision", serde_json::json!(2)),
    ] {
        let mut forged = command.clone();
        let mut payload = serde_json::to_value(&forged.run).unwrap();
        payload[field] = value;
        forged.run = serde_json::from_value(payload).unwrap();
        assert_eq!(
            store
                .execute(DeliveryCommand::AppendWorkRun(Box::new(forged)))
                .unwrap_err()
                .code(),
            DeliveryStoreErrorCode::InvalidStoreOptions,
            "unproven initial {field}"
        );
    }
    let first = store
        .execute(DeliveryCommand::AppendWorkRun(Box::new(command.clone())))
        .unwrap();
    assert_eq!(first.snapshot.revision(), delivery.revision() + 1);
    let bindings = &first.snapshot.snapshot().session_bindings;
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].work_run_id, run.id);
    assert_eq!(bindings[0].work_item_id, run.work_item_id);
    assert_eq!(bindings[0].execution_job_id, run.execution_job_id);
    assert_eq!(
        bindings[0].worker_session_id.as_ref(),
        Some(&run.worker_session_id)
    );
    assert!(bindings[0].codex_thread_id.is_none());
    assert_eq!(bindings[0].lease_id.as_ref(), Some(&run.lease_id));
    assert_eq!(
        bindings[0].fencing_token.as_ref().map(|fence| &fence.0),
        Some(&run.fencing_token)
    );
    assert_eq!(
        bindings[0].source_provenance.reference(),
        "workrun.appended"
    );
    assert_eq!(first.snapshot.snapshot().work_run_aggregate.runs, vec![run]);
    let replay = store
        .execute(DeliveryCommand::AppendWorkRun(Box::new(command.clone())))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.snapshot, first.snapshot);
    for field in ["state", "revision", "candidateDigest"] {
        let mut changed = command.clone();
        let mut payload = serde_json::to_value(&changed.run).unwrap();
        payload[field] = match field {
            "state" => "running".into(),
            "revision" => 2.into(),
            _ => format!("sha256:{}", "e".repeat(64)).into(),
        };
        changed.run = serde_json::from_value(payload).unwrap();
        assert_eq!(
            store
                .execute(DeliveryCommand::AppendWorkRun(Box::new(changed)))
                .unwrap_err()
                .code(),
            DeliveryStoreErrorCode::RequestConflict,
            "{field}"
        );
    }
    let replay_after_rejections = store
        .execute(DeliveryCommand::AppendWorkRun(Box::new(command.clone())))
        .unwrap();
    assert_eq!(replay_after_rejections.snapshot, first.snapshot);

    let mut stale = command;
    stale.request_id = RequestId(id("req", 102));
    stale.request_digest = "d".repeat(64);
    assert_eq!(
        store
            .execute(DeliveryCommand::AppendWorkRun(Box::new(stale)))
            .unwrap_err()
            .code(),
        DeliveryStoreErrorCode::RevisionConflict
    );
    Box::new(storage).close().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn delivery_decoder_rejects_invalid_workrun_aggregate_before_any_write() {
    let source = fixture();
    let original = serde_json::to_value(source.snapshot()).unwrap();
    let mut bad = original.clone();
    bad["workRunAggregate"]["contract"]["requiredHumanAuthority"] = "invented".into();
    assert!(Delivery::decode_json(&serde_json::to_vec(&bad).unwrap()).is_err());
    let mut bad = original;
    bad["workRunAggregate"]["items"][0]["workContractId"] = id("wct", 99).into();
    assert!(Delivery::decode_json(&serde_json::to_vec(&bad).unwrap()).is_err());
}
