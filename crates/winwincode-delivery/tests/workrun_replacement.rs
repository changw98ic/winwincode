// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::too_many_lines)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use winwincode_delivery::application::CoordinationErrorCode;
use winwincode_delivery::application::session_binding::{
    DeliveryExecutionAttemptReplacement, SessionBindingAuthority, SessionBindingIdentity,
    accept_replacement_worker_session_with_authority,
};
use winwincode_delivery::application::stage::{
    CancelAcknowledgement, acknowledge_cancel, request_cancel,
};
use winwincode_delivery::domain::{Delivery, DeliveryValidationErrorCode};
use winwincode_delivery::store::{
    AcceptDeliveryWorkerSession, AppendDeliveryWorkRun, CreateDelivery, CreateDeliveryWorkItems,
    DeliveryCommand, DeliveryCommandPort, DeliveryStore, DeliveryStoreErrorCode,
    InMemoryDeliveryJournal, ReplaceDeliveryExecutionAttempt, ReportDeliveryCodexThread,
};
use winwincode_domain::{
    CodexThreadId, ExecutionJobId, ExecutionMessageId, ExecutionSequence, Instant, LeaseId,
    ProductSessionId, RequestId, Revision, Sha256Digest, WorkContractId, WorkItemId, WorkRunId,
    WorkerId, WorkerInstanceId, WorkerSessionId,
};
use winwincode_storage::{
    DispatchResultRequest, DispatchResultStatus, EXECUTION_PROTOCOL_VERSION,
    ExecutionAdmissionBoundary, ExecutionAdmissionLimits, ExecutionAdmissionPolicy,
    ExecutionJobSubmission, ExecutionLeaseTerminalOutcome, ExecutionLeaseTerminalRequest,
    ExecutionQueueScope, ExecutionRepositoryAccess, ExecutionReservationRequest,
    ExecutionReservationStart, ExecutionScopeReplacementAuthority,
    RepositorySchedulerCancellationRequest, RepositorySchedulerClaimRequest,
    RepositorySchedulerScope, RepositorySchedulerTerminalRequest, SqliteStorage, StorageErrorKind,
    WorkerAuthenticationIdentity, WorkerHeartbeatRequest, WorkerPlatform, WorkerPoolId,
    WorkerRegistrationRequest, WorkerSlotAuthority, WorkerSlotOpenRequest,
    WorkerSlotResourceLimits, WorkerSlotResources,
};

type WorkRunMutation = fn(&mut winwincode_domain::WorkRun);

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn id(prefix: &str, seed: u64) -> String {
    format!("{prefix}_{seed:026}")
}
fn at(second: u64) -> Instant {
    Instant(format!("2027-10-01T10:00:{second:02}.000Z"))
}
fn directory() -> PathBuf {
    let n = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "winwincode-delivery-replacement-{}-{n}",
        std::process::id()
    ))
}

fn repository() -> RepositorySchedulerScope {
    RepositorySchedulerScope {
        organization_id: winwincode_domain::OrganizationId(id("org", 1)),
        workspace_id: winwincode_domain::WorkspaceId(id("wsp", 2)),
        project_id: winwincode_domain::ProjectId(id("prj", 3)),
        repository_id: winwincode_domain::RepositoryId(id("rep", 4)),
    }
}
fn queue_scope() -> ExecutionQueueScope {
    let r = repository();
    ExecutionQueueScope {
        organization_id: r.organization_id,
        workspace_id: r.workspace_id,
        project_id: r.project_id,
        repository_id: r.repository_id,
        product_session_id: ProductSessionId(id("psn", 5)),
        delivery_id: Some(winwincode_domain::DeliveryId(id("dlv", 1))),
    }
}
fn claim(
    request: u64,
    generation: &str,
    instance: u64,
    issued: u64,
) -> RepositorySchedulerClaimRequest {
    RepositorySchedulerClaimRequest {
        scope: repository(),
        request_id: RequestId(id("req", request)),
        scheduler_generation: generation.into(),
        worker_id: WorkerId(id("wrk", 7)),
        worker_instance_id: WorkerInstanceId(id("wki", instance)),
        issued_at: at(issued),
        expires_at: at(if instance == 8 { 24 } else { 50 }),
    }
}
fn register(storage: &mut SqliteStorage, instance: u64, seed: u64, started: u64) {
    let worker_id = WorkerId(id("wrk", 7));
    let worker_instance_id = WorkerInstanceId(id("wki", instance));
    storage
        .execution_registry()
        .unwrap()
        .register_worker(&WorkerRegistrationRequest {
            authentication_identity: WorkerAuthenticationIdentity::LocalEmbedded {
                control_plane_principal: "fixture-control-plane".into(),
            },
            protocol_version: EXECUTION_PROTOCOL_VERSION.into(),
            platform: WorkerPlatform::Aarch64AppleDarwin,
            capabilities: vec!["codex".into()],
            capability_digest: Sha256Digest(format!("sha256:{}", "b".repeat(64))),
            security_zone: "local".into(),
            max_slots: 2,
            message_id: ExecutionMessageId(id("xmsg", seed)),
            request_id: RequestId(id("req", seed)),
            sent_at: at(started),
            started_at: at(started),
            worker_id: worker_id.clone(),
            worker_instance_id: worker_instance_id.clone(),
        })
        .unwrap();
    storage
        .execution_registry()
        .unwrap()
        .record_heartbeat(&WorkerHeartbeatRequest {
            active_leases: Vec::new(),
            available_slots: 2,
            heartbeat_sequence: ExecutionSequence(1),
            max_slots: 2,
            running_slots: 0,
            message_id: ExecutionMessageId(id("xmsg", seed + 1)),
            observed_at: at(started + 1),
            sent_at: at(started + 1),
            worker_id,
            worker_instance_id,
        })
        .unwrap();
}
fn prepare_admission(storage: &mut SqliteStorage, job_id: &ExecutionJobId) {
    prepare_admission_with_seed(storage, job_id, 31);
}
fn prepare_admission_with_seed(
    storage: &mut SqliteStorage,
    job_id: &ExecutionJobId,
    request_seed: u64,
) {
    let scope = queue_scope();
    let pool = WorkerPoolId(id("wpl", 30));
    let limits = ExecutionAdmissionLimits {
        max_concurrent: 2,
        max_queued: 2,
        token_budget: 10_000,
        cost_budget_microunits: 100_000,
        max_runtime_millis: 60_000,
    };
    let boundaries = [
        ExecutionAdmissionBoundary::Organization {
            organization_id: scope.organization_id.clone(),
        },
        ExecutionAdmissionBoundary::Project {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
        },
        ExecutionAdmissionBoundary::Repository {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            repository_id: scope.repository_id.clone(),
        },
        ExecutionAdmissionBoundary::Delivery {
            organization_id: scope.organization_id.clone(),
            delivery_id: scope.delivery_id.clone().unwrap(),
        },
        ExecutionAdmissionBoundary::ProductSession {
            organization_id: scope.organization_id.clone(),
            project_id: scope.project_id.clone(),
            product_session_id: scope.product_session_id.clone(),
        },
        ExecutionAdmissionBoundary::WorkerPool {
            organization_id: scope.organization_id.clone(),
            worker_pool_id: pool.clone(),
        },
    ];
    let mut admission = storage.execution_admission().unwrap();
    for boundary in boundaries {
        admission
            .configure_policy(&ExecutionAdmissionPolicy { boundary, limits })
            .unwrap();
    }
    admission
        .reserve(&ExecutionReservationRequest {
            scope: scope.clone(),
            user_id: winwincode_domain::UserId(id("usr", 31)),
            worker_pool_id: pool.clone(),
            job_id: job_id.clone(),
            request_id: RequestId(id("req", request_seed)),
            repository_access: ExecutionRepositoryAccess::ReadOnly,
            reserved_tokens: 100,
            reserved_cost_microunits: 1_000,
            runtime_limit_millis: 30_000,
            submitted_at: at(4),
        })
        .unwrap();
    admission
        .start(&ExecutionReservationStart {
            scope,
            worker_pool_id: pool,
            job_id: job_id.clone(),
            request_id: RequestId(id("req", request_seed + 1)),
            expected_revision: 1,
            started_at: at(5),
        })
        .unwrap();
}

fn submit(storage: &mut SqliteStorage) {
    submit_job(storage, 9, 6, 6);
}
fn submit_job(storage: &mut SqliteStorage, work_run_seed: u64, job_seed: u64, request_seed: u64) {
    let scope = queue_scope();
    let work_run = id("wrn", work_run_seed);
    let job = id("job", job_seed);
    let payload = serde_json::json!({
        "executionProfile": "executor", "attempt": 1, "jobId": job, "payloadDigest": format!("sha256:{}", "a".repeat(64)), "scope": {
            "kind": "work-run",
            "productSessionId": scope.product_session_id.0, "workContractId": id("wct", 1),
            "workContractRevision": 1, "workItemId": id("wit", 1), "workItemRevision": 1, "workRunId": work_run, "attempt": 1
        }
    });
    storage
        .execution_queue()
        .unwrap()
        .submit(&ExecutionJobSubmission {
            scope,
            job_id: ExecutionJobId(job),
            request_id: RequestId(id("req", request_seed)),
            payload_digest: Sha256Digest(format!("sha256:{}", "a".repeat(64))),
            dispatch_payload: serde_json::to_vec(&payload).unwrap(),
            attempt: 1,
            dependencies: Vec::new(),
            work_run_id: Some(WorkRunId(work_run)),
            submitted_at: at(1),
        })
        .unwrap();
}
fn dispatch_result(
    job: &winwincode_storage::RepositorySchedulerClaimReceipt,
    request: u64,
    session: u64,
    second: u64,
) -> DispatchResultRequest {
    DispatchResultRequest {
        checked_at: at(second),
        expires_at: job.lease.expires_at.clone(),
        fencing_token: job.lease.fencing_token.clone(),
        issued_at: job.lease.issued_at.clone(),
        job_id: job.lease.job_id.clone(),
        lease_id: job.lease.lease_id.clone(),
        message_id: ExecutionMessageId(id("xmsg", request)),
        payload_digest: job.lease.payload_digest.clone(),
        request_id: RequestId(id("req", request)),
        sent_at: at(second),
        status: DispatchResultStatus::Accepted,
        attempt: job.lease.attempt,
        error: None,
        worker_id: job.lease.worker_id.clone(),
        worker_instance_id: job.lease.worker_instance_id.clone(),
        worker_session_id: Some(WorkerSessionId(id("wsn", session))),
    }
}

fn initial_delivery() -> Delivery {
    let source: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let dispatch = source["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "job.dispatch")
        .unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/delivery-main.json")).unwrap();
    value["id"] = id("dlv", 1).into();
    value["spec"]["deliveryId"] = id("dlv", 1).into();
    value["revision"] = 1.into();
    value["status"] = "draft".into();
    value["updatedAtMillis"] = value["createdAtMillis"].clone();
    for field in [
        "tasks",
        "stageRuns",
        "sessionBindings",
        "attentionItems",
        "evidence",
    ] {
        value[field] = serde_json::json!([]);
    }
    value["verdict"] = serde_json::Value::Null;
    value["workRunAggregate"]["contract"] = dispatch["job"]["workInput"]["workContract"].clone();
    value["workRunAggregate"]["items"] = serde_json::json!([]);
    value["workRunAggregate"]["runs"] = serde_json::json!([]);
    Delivery::decode_json(&serde_json::to_vec(&value).unwrap()).unwrap()
}

fn initial_work_item() -> winwincode_domain::WorkItem {
    let source: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../tests/fixtures/contracts/execution-port.valid.json"
    ))
    .unwrap();
    let dispatch = source["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "job.dispatch")
        .unwrap();
    serde_json::from_value(dispatch["job"]["workInput"]["workItem"].clone()).unwrap()
}

fn identity() -> SessionBindingIdentity {
    SessionBindingIdentity {
        delivery_id: winwincode_domain::DeliveryId(id("dlv", 1)),
        work_contract_id: WorkContractId(id("wct", 1)),
        work_contract_revision: Revision(1),
        work_item_id: WorkItemId(id("wit", 1)),
        work_item_revision: Revision(1),
        work_run_id: WorkRunId(id("wrn", 9)),
        product_session_id: ProductSessionId(id("psn", 5)),
        execution_job_id: ExecutionJobId(id("job", 6)),
    }
}

fn assert_tampered_predecessor_rejected(
    bound: &Delivery,
    replacement: &DeliveryExecutionAttemptReplacement,
    successor_authority: &SessionBindingAuthority,
    field: &str,
    mutate: fn(&mut winwincode_domain::WorkRun),
) {
    let mut snapshot = bound.clone().into_snapshot();
    let predecessor = snapshot
        .work_run_aggregate
        .runs
        .iter_mut()
        .find(|run| run.id == identity().work_run_id)
        .unwrap();
    mutate(predecessor);
    let tampered = Delivery::try_from_snapshot(snapshot).unwrap();
    let before = tampered.encode_json().unwrap();
    let error = accept_replacement_worker_session_with_authority(
        &tampered,
        tampered.revision(),
        &identity(),
        replacement,
        successor_authority,
        tampered.snapshot().updated_at_millis + 1,
    )
    .expect_err("replacement must reject a tampered predecessor WorkRun");
    assert_eq!(
        error.code(),
        CoordinationErrorCode::BindingConflict,
        "tampered {field} must be rejected"
    );
    assert_eq!(
        tampered.encode_json().unwrap(),
        before,
        "rejecting tampered {field} must not mutate the input Delivery"
    );
}

#[test]
fn real_scheduler_replacement_rotates_delivery_workrun_and_rejects_old_authority_after_restart() {
    let root = directory();
    let mut storage = SqliteStorage::open(&root).unwrap();
    submit(&mut storage);
    register(&mut storage, 8, 100, 2);
    let original = storage
        .repository_scheduler()
        .unwrap()
        .claim_next(&claim(110, "boot-old", 8, 4))
        .unwrap()
        .unwrap();
    let old_dispatch = dispatch_result(&original, 111, 112, 5);
    storage
        .repository_scheduler()
        .unwrap()
        .record_dispatch_result(
            &winwincode_storage::RepositorySchedulerDispatchResultRequest {
                scope: repository(),
                dispatch: old_dispatch.clone(),
            },
        )
        .unwrap();

    let old_authority = storage
        .execution_registry()
        .unwrap()
        .load_dispatch_authority(&original.job.job_id)
        .unwrap()
        .unwrap();
    let old_proof = storage
        .execution_registry()
        .unwrap()
        .load_dispatch_work_run_proof(&old_authority, &at(6))
        .unwrap();
    let run_value = serde_json::json!({
        "schemaVersion":"winwincode/v1", "id":id("wrn", 9), "workContractId":id("wct", 1), "contractRevision":1,
        "workItemId":id("wit", 1), "workItemRevision":1, "revision":1, "state":"leased", "executionJobId":original.lease.job_id,
        "attempt":1, "workerId":original.lease.worker_id, "workerInstanceId":original.lease.worker_instance_id,
        "workerSessionId":id("wsn", 112), "leaseId":original.lease.lease_id, "fencingToken":original.lease.fencing_token,
        "productSessionId":id("psn", 5), "codexThreadId":null, "candidateDigest":null
    });
    let run: winwincode_domain::WorkRun = serde_json::from_value(run_value).unwrap();
    old_proof.verify_work_run(&run, &id("dlv", 1)).unwrap();

    let journal = Arc::new(InMemoryDeliveryJournal::new());
    let store = DeliveryStore::new(Arc::clone(&journal));
    let empty = initial_delivery();
    store
        .execute(DeliveryCommand::Create(CreateDelivery {
            request_id: RequestId(id("req", 1)),
            request_digest: "c".repeat(64),
            snapshot: empty.clone(),
        }))
        .unwrap();
    let with_items = store
        .execute(DeliveryCommand::CreateWorkItems(Box::new(
            CreateDeliveryWorkItems {
                delivery_id: empty.id().clone(),
                request_id: RequestId(id("req", 2)),
                request_digest: "d".repeat(64),
                expected_revision: empty.revision(),
                contract_revision: 1,
                items: vec![initial_work_item()],
                now_millis: empty.snapshot().updated_at_millis + 1,
            },
        )))
        .unwrap()
        .snapshot;
    let append_work_run = AppendDeliveryWorkRun {
        delivery_id: empty.id().clone(),
        request_id: RequestId(id("req", 3)),
        request_digest: "e".repeat(64),
        expected_revision: with_items.revision(),
        run: run.clone(),
        authority: old_authority.clone(),
        proof: old_proof,
        now_millis: with_items.snapshot().updated_at_millis + 1,
    };
    let appended = store
        .execute(DeliveryCommand::AppendWorkRun(Box::new(
            append_work_run.clone(),
        )))
        .unwrap()
        .snapshot;
    let old_session_authority = SessionBindingAuthority::from_execution_port(
        original.lease.worker_id.clone(),
        original.lease.worker_instance_id.clone(),
        original.lease.lease_id.clone(),
        1,
        original.lease.fencing_token.clone(),
        WorkerSessionId(id("wsn", 112)),
        old_dispatch.message_id.clone(),
    );
    let bound = store
        .execute(DeliveryCommand::AcceptWorkerSession(Box::new(
            AcceptDeliveryWorkerSession {
                request_id: RequestId(id("req", 5)),
                request_digest: "f".repeat(64),
                expected_revision: appended.revision(),
                identity: identity(),
                authority: old_session_authority,
                now_millis: appended.snapshot().updated_at_millis + 1,
            },
        )))
        .unwrap()
        .snapshot;
    let report_authority = SessionBindingAuthority::from_execution_port(
        original.lease.worker_id.clone(),
        original.lease.worker_instance_id.clone(),
        original.lease.lease_id.clone(),
        1,
        original.lease.fencing_token.clone(),
        WorkerSessionId(id("wsn", 112)),
        old_dispatch.message_id.clone(),
    );
    let report = ReportDeliveryCodexThread {
        request_id: RequestId(id("req", 6)),
        request_digest: "1".repeat(64),
        expected_revision: bound.revision(),
        identity: identity(),
        authority: report_authority.clone(),
        codex_thread_id: CodexThreadId(id("cdx", 113)),
        now_millis: bound.snapshot().updated_at_millis + 1,
    };
    let bound = store
        .execute(DeliveryCommand::ReportCodexThread(Box::new(report.clone())))
        .unwrap()
        .snapshot;
    let bound_run = bound
        .snapshot()
        .work_run_aggregate
        .runs
        .iter()
        .find(|run| run.id == identity().work_run_id)
        .unwrap();
    let bound_codex_thread = CodexThreadId(id("cdx", 113));
    assert_eq!(bound_run.state, winwincode_domain::WorkRunState::Running);
    assert_eq!(bound_run.revision.0, 2);
    assert_eq!(
        bound_run.codex_thread_id.as_ref(),
        Some(&bound_codex_thread)
    );
    let bound_binding = bound
        .snapshot()
        .session_bindings
        .iter()
        .find(|binding| binding.work_run_id == identity().work_run_id)
        .unwrap();
    assert_eq!(
        bound_binding.codex_thread_id.as_ref(),
        Some(&bound_codex_thread)
    );
    assert_eq!(bound_binding.attempt, 1);

    // Cancellation is addressed by the durable WorkRun identity, not by the
    // historical StageRun projection.  The acknowledgement is intentionally
    // non-terminal: Delivery remains byte-for-byte unchanged.
    let cancel_intent = request_cancel(&bound, bound.revision(), &identity().work_run_id)
        .expect("accepted WorkRun can be cancelled");
    let after_cancel_ack = acknowledge_cancel(
        &bound,
        &cancel_intent,
        &CancelAcknowledgement {
            work_run_id: cancel_intent.work_run_id.clone(),
            execution_job_id: cancel_intent.execution_job_id.clone(),
            attempt: cancel_intent.attempt,
            worker_session_id: cancel_intent.worker_session_id.clone(),
        },
    )
    .expect("matching cancellation acknowledgement");
    assert_eq!(after_cancel_ack, bound);
    assert_eq!(after_cancel_ack.revision(), bound.revision());
    assert_eq!(
        after_cancel_ack
            .snapshot()
            .work_run_aggregate
            .runs
            .iter()
            .find(|run| run.id == identity().work_run_id)
            .expect("predecessor WorkRun")
            .state,
        winwincode_domain::WorkRunState::Running
    );
    let current_revision_before_append_replay = bound.revision();
    let current_run_count_before_append_replay = bound.snapshot().work_run_aggregate.runs.len();
    let current_binding_count_before_append_replay = bound.snapshot().session_bindings.len();
    let append_replay = store
        .execute(DeliveryCommand::AppendWorkRun(Box::new(
            append_work_run.clone(),
        )))
        .unwrap();
    assert!(append_replay.replayed);
    assert_eq!(append_replay.snapshot, appended);
    let after_append_replay = store
        .execute(DeliveryCommand::ReportCodexThread(Box::new(report.clone())))
        .unwrap();
    assert!(after_append_replay.replayed);
    assert_eq!(after_append_replay.snapshot, bound);
    assert_eq!(
        after_append_replay.snapshot.revision(),
        current_revision_before_append_replay
    );
    assert_eq!(
        after_append_replay
            .snapshot
            .snapshot()
            .work_run_aggregate
            .runs
            .len(),
        current_run_count_before_append_replay
    );
    assert_eq!(
        after_append_replay
            .snapshot
            .snapshot()
            .session_bindings
            .len(),
        current_binding_count_before_append_replay
    );
    let mut changed_append = append_work_run.clone();
    changed_append.run.worker_instance_id = WorkerInstanceId(id("wki", 88));
    assert_eq!(
        store
            .execute(DeliveryCommand::AppendWorkRun(Box::new(changed_append)))
            .unwrap_err()
            .code(),
        DeliveryStoreErrorCode::InvalidStoreOptions
    );
    let after_changed_append = store
        .execute(DeliveryCommand::ReportCodexThread(Box::new(report.clone())))
        .unwrap();
    assert!(after_changed_append.replayed);
    assert_eq!(after_changed_append.snapshot, bound);
    let report_replay = store
        .execute(DeliveryCommand::ReportCodexThread(Box::new(report.clone())))
        .unwrap();
    assert!(report_replay.replayed);
    assert_eq!(report_replay.snapshot, bound);
    let changed_report = ReportDeliveryCodexThread {
        request_id: RequestId(id("req", 8)),
        request_digest: "2".repeat(64),
        expected_revision: bound.revision(),
        identity: identity(),
        authority: report_authority,
        codex_thread_id: CodexThreadId(id("cdx", 114)),
        now_millis: bound.snapshot().updated_at_millis + 1,
    };
    assert_eq!(
        store
            .execute(DeliveryCommand::ReportCodexThread(Box::new(changed_report)))
            .unwrap_err()
            .code(),
        DeliveryStoreErrorCode::InvalidStoreOptions
    );
    let unchanged_after_changed_report = store
        .execute(DeliveryCommand::ReportCodexThread(Box::new(report)))
        .unwrap();
    assert!(unchanged_after_changed_report.replayed);
    assert_eq!(unchanged_after_changed_report.snapshot, bound);

    storage.execution_registry().unwrap();
    storage.execution_admission().unwrap();
    prepare_admission(&mut storage, &original.lease.job_id);
    storage
        .worker_session_slots()
        .unwrap()
        .configure_resources(
            &original.lease.worker_id,
            &original.lease.worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 100,
                max_disk_bytes: 100,
                max_processes: 1,
            },
        )
        .unwrap();
    storage
        .worker_session_slots()
        .unwrap()
        .open(&WorkerSlotOpenRequest {
            authority: WorkerSlotAuthority {
                worker_id: original.lease.worker_id.clone(),
                worker_instance_id: original.lease.worker_instance_id.clone(),
                worker_session_id: WorkerSessionId(id("wsn", 112)),
                codex_thread_id: CodexThreadId(id("cdx", 113)),
                job_id: original.lease.job_id.clone(),
                lease_id: original.lease.lease_id.clone(),
                attempt: 1,
                fencing_token: original.lease.fencing_token.clone(),
            },
            resources: WorkerSlotResources {
                memory_bytes: 1,
                disk_bytes: 1,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 7)),
            opened_at: at(7),
        })
        .unwrap();
    register(&mut storage, 9, 120, 7);
    let replacement = storage
        .repository_scheduler()
        .unwrap()
        .claim_next(&claim(130, "boot-new", 9, 24))
        .unwrap()
        .unwrap();
    let replacement_authority: ExecutionScopeReplacementAuthority = storage
        .load_execution_scope_replacement(&replacement.job.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(replacement_authority.previous_attempt(), 1);
    assert_eq!(replacement_authority.replacement_attempt(), 2);
    assert_ne!(
        replacement_authority.work_run_id().unwrap(),
        &WorkRunId(id("wrn", 9))
    );
    assert_eq!(
        replacement_authority.work_run_id(),
        replacement.job.work_run_id.as_ref()
    );
    drop(storage);
    let mut restarted = SqliteStorage::open(&root).unwrap();
    let opaque = restarted
        .load_execution_scope_replacement(&replacement.job.job_id)
        .unwrap()
        .unwrap();
    let replacement_dispatch = dispatch_result(&replacement, 140, 141, 25);
    restarted
        .repository_scheduler()
        .unwrap()
        .record_dispatch_result(
            &winwincode_storage::RepositorySchedulerDispatchResultRequest {
                scope: repository(),
                dispatch: replacement_dispatch.clone(),
            },
        )
        .unwrap();
    let successor_authority = SessionBindingAuthority::from_execution_port(
        replacement.lease.worker_id.clone(),
        replacement.lease.worker_instance_id.clone(),
        replacement.lease.lease_id.clone(),
        2,
        replacement.lease.fencing_token.clone(),
        WorkerSessionId(id("wsn", 141)),
        replacement_dispatch.message_id.clone(),
    );
    let replacement_for_delivery = DeliveryExecutionAttemptReplacement::from_scheduler(&opaque);
    let mut changed_item_snapshot = bound.clone().into_snapshot();
    changed_item_snapshot.work_run_aggregate.items[0].revision = Revision(2);
    changed_item_snapshot.work_run_aggregate.items[0].goal = "changed acceptance".into();
    let changed_item = Delivery::try_from_snapshot(changed_item_snapshot)
        .expect("item revision/goal change remains a valid aggregate shape");
    let changed_item_before = changed_item.encode_json().unwrap();
    let changed_item_error = accept_replacement_worker_session_with_authority(
        &changed_item,
        changed_item.revision(),
        &identity(),
        &replacement_for_delivery,
        &successor_authority,
        changed_item.snapshot().updated_at_millis + 1,
    )
    .expect_err("replacement must reject a changed WorkItem revision and goal");
    assert_eq!(
        changed_item_error.code(),
        CoordinationErrorCode::BindingConflict
    );
    assert_eq!(changed_item.encode_json().unwrap(), changed_item_before);
    let predecessor_mutations: [(&str, WorkRunMutation); 3] = [
        ("workerInstanceId", |run| {
            run.worker_instance_id = WorkerInstanceId(id("wki", 88));
        }),
        ("leaseId", |run| run.lease_id = LeaseId(id("lse", 88))),
        ("fencingToken", |run| run.fencing_token = "88".into()),
    ];
    for (field, mutate) in predecessor_mutations {
        assert_tampered_predecessor_rejected(
            &bound,
            &replacement_for_delivery,
            &successor_authority,
            field,
            mutate,
        );
    }
    assert_ne!(opaque.work_run_id().unwrap(), &identity().work_run_id);
    assert_eq!(opaque.work_run_id(), replacement.job.work_run_id.as_ref());
    assert_eq!(
        opaque.scope().delivery_id.as_ref().unwrap(),
        &identity().delivery_id
    );
    assert_eq!(
        opaque.scope().product_session_id,
        identity().product_session_id
    );
    let command = ReplaceDeliveryExecutionAttempt {
        request_id: opaque.receipt_id().clone(),
        request_digest: opaque
            .receipt_digest()
            .0
            .strip_prefix("sha256:")
            .expect("replacement request digest prefix")
            .to_owned(),
        expected_revision: bound.revision(),
        identity: identity(),
        replacement: replacement_for_delivery,
        successor_authority,
        now_millis: bound.snapshot().updated_at_millis + 1,
    };
    let first = store
        .execute(DeliveryCommand::ReplaceExecutionAttempt(Box::new(
            command.clone(),
        )))
        .unwrap();
    assert!(!first.replayed);
    assert_eq!(first.snapshot.snapshot().work_run_aggregate.runs.len(), 2);
    assert_eq!(
        first.snapshot.snapshot().work_run_aggregate.runs[0].state,
        winwincode_domain::WorkRunState::Failed
    );
    assert_eq!(
        first.snapshot.snapshot().work_run_aggregate.runs[0]
            .revision
            .0,
        3
    );
    assert_eq!(
        first.snapshot.snapshot().work_run_aggregate.runs[0]
            .codex_thread_id
            .as_ref(),
        Some(&bound_codex_thread)
    );
    assert_eq!(
        first.snapshot.snapshot().work_run_aggregate.runs[1].attempt,
        2
    );
    assert!(
        acknowledge_cancel(
            &first.snapshot,
            &cancel_intent,
            &CancelAcknowledgement {
                work_run_id: cancel_intent.work_run_id.clone(),
                execution_job_id: cancel_intent.execution_job_id.clone(),
                attempt: cancel_intent.attempt,
                worker_session_id: cancel_intent.worker_session_id.clone(),
            },
        )
        .is_err(),
        "a predecessor cancellation acknowledgement must be rejected after replacement"
    );
    let replay = store
        .execute(DeliveryCommand::ReplaceExecutionAttempt(Box::new(
            command.clone(),
        )))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.snapshot, first.snapshot);

    let persisted_before_negative = replay.snapshot.clone();
    let mut duplicate_work_run_binding = persisted_before_negative.clone().into_snapshot();
    let mut duplicate_binding = duplicate_work_run_binding.session_bindings[0].clone();
    duplicate_binding.id =
        winwincode_delivery::domain::SessionBindingId::new(id("sbn", 20)).unwrap();
    duplicate_work_run_binding
        .session_bindings
        .push(duplicate_binding);
    let duplicate_work_run_error = Delivery::try_from_snapshot(duplicate_work_run_binding)
        .expect_err("a WorkRun cannot have two session bindings");
    assert_eq!(
        duplicate_work_run_error.code(),
        DeliveryValidationErrorCode::DuplicateId
    );
    assert_eq!(
        duplicate_work_run_error.path(),
        "delivery.sessionBindings.workRunId"
    );

    let mut duplicate_job_attempt = persisted_before_negative.clone().into_snapshot();
    let mut extra_item = duplicate_job_attempt.work_run_aggregate.items[0].clone();
    extra_item.id = WorkItemId(id("wit", 2));
    duplicate_job_attempt
        .work_run_aggregate
        .items
        .push(extra_item.clone());
    let mut extra_run = duplicate_job_attempt.work_run_aggregate.runs[0].clone();
    extra_run.id = WorkRunId(id("wrn", 11));
    extra_run.work_item_id = extra_item.id.clone();
    duplicate_job_attempt
        .work_run_aggregate
        .runs
        .push(extra_run);
    let mut duplicate_attempt_binding = duplicate_job_attempt.session_bindings[0].clone();
    duplicate_attempt_binding.id =
        winwincode_delivery::domain::SessionBindingId::new(id("sbn", 21)).unwrap();
    duplicate_attempt_binding.work_run_id = WorkRunId(id("wrn", 11));
    duplicate_attempt_binding.work_item_id = extra_item.id;
    duplicate_attempt_binding.worker_session_id = Some(WorkerSessionId(id("wsn", 142)));
    duplicate_attempt_binding.codex_thread_id = Some(CodexThreadId(id("cdx", 114)));
    duplicate_attempt_binding.lease_id = Some(LeaseId(id("lse", 115)));
    duplicate_job_attempt
        .session_bindings
        .push(duplicate_attempt_binding);
    let duplicate_job_attempt_error = Delivery::try_from_snapshot(duplicate_job_attempt)
        .expect_err("one execution job attempt cannot bind two WorkRuns");
    assert_eq!(
        duplicate_job_attempt_error.code(),
        DeliveryValidationErrorCode::RelationshipMismatch
    );
    assert_eq!(
        duplicate_job_attempt_error.path(),
        "delivery.workRunAggregate"
    );

    let unchanged = store
        .execute(DeliveryCommand::ReplaceExecutionAttempt(Box::new(
            command.clone(),
        )))
        .unwrap();
    assert!(unchanged.replayed);
    assert_eq!(unchanged.snapshot, persisted_before_negative);

    let mut changed = command;
    changed.successor_authority = SessionBindingAuthority::from_execution_port(
        WorkerId(id("wrk", 7)),
        WorkerInstanceId(id("wki", 9)),
        replacement.lease.lease_id.clone(),
        2,
        replacement.lease.fencing_token.clone(),
        WorkerSessionId(id("wsn", 999)),
        ExecutionMessageId(id("xmsg", 999)),
    );
    assert_eq!(
        store
            .execute(DeliveryCommand::ReplaceExecutionAttempt(Box::new(changed)))
            .unwrap_err()
            .code(),
        DeliveryStoreErrorCode::InvalidStoreOptions
    );
    let old_lease = original.lease;
    assert_eq!(
        restarted
            .execution_registry()
            .unwrap()
            .finish_execution_lease(&ExecutionLeaseTerminalRequest {
                job_id: old_lease.job_id,
                lease_id: old_lease.lease_id,
                worker_id: old_lease.worker_id,
                worker_instance_id: old_lease.worker_instance_id,
                attempt: 1,
                fencing_token: old_lease.fencing_token,
                outcome: ExecutionLeaseTerminalOutcome::Completed,
                terminal_at: at(26),
                request_id: RequestId(id("req", 150))
            })
            .unwrap_err()
            .kind(),
        StorageErrorKind::InvalidInput
    );
    drop(restarted);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sqlite_cancellation_selects_workrun_and_preserves_sibling_running_job() {
    let root = directory();
    let mut storage = SqliteStorage::open(&root).unwrap();
    submit_job(&mut storage, 9, 6, 6);
    submit_job(&mut storage, 10, 7, 7);
    register(&mut storage, 8, 100, 2);
    prepare_admission_with_seed(&mut storage, &ExecutionJobId(id("job", 6)), 31);
    prepare_admission_with_seed(&mut storage, &ExecutionJobId(id("job", 7)), 33);

    let first = storage
        .repository_scheduler()
        .unwrap()
        .claim_next(&claim(110, "boot-two", 8, 4))
        .unwrap()
        .unwrap();
    let first_dispatch = dispatch_result(&first, 111, 112, 5);
    storage
        .repository_scheduler()
        .unwrap()
        .record_dispatch_result(
            &winwincode_storage::RepositorySchedulerDispatchResultRequest {
                scope: repository(),
                dispatch: first_dispatch,
            },
        )
        .unwrap();
    let second = storage
        .repository_scheduler()
        .unwrap()
        .claim_next(&claim(115, "boot-two", 8, 6))
        .unwrap()
        .unwrap();
    let second_dispatch = dispatch_result(&second, 116, 117, 7);
    storage
        .repository_scheduler()
        .unwrap()
        .record_dispatch_result(
            &winwincode_storage::RepositorySchedulerDispatchResultRequest {
                scope: repository(),
                dispatch: second_dispatch,
            },
        )
        .unwrap();
    let authority = storage
        .execution_registry()
        .unwrap()
        .load_dispatch_authority(&second.job.job_id)
        .unwrap()
        .unwrap();
    storage.execution_admission().unwrap();
    storage
        .worker_session_slots()
        .unwrap()
        .configure_resources(
            &authority.lease().worker_id,
            &authority.lease().worker_instance_id,
            WorkerSlotResourceLimits {
                max_memory_bytes: 100,
                max_disk_bytes: 100,
                max_processes: 1,
            },
        )
        .unwrap();
    let jobs_before = storage
        .repository_scheduler()
        .unwrap()
        .list_jobs(
            &repository(),
            &[winwincode_storage::ExecutionJobState::Running],
        )
        .unwrap();
    let cancellation_request = RepositorySchedulerCancellationRequest {
        scope: repository(),
        job_id: second.job.job_id.clone(),
        request_id: RequestId(id("req", 120)),
        expected_revision: second.job.revision + 1,
        requested_at: at(9),
    };
    let pending = storage
        .repository_scheduler()
        .unwrap()
        .request_cancellation_for_work_run(&cancellation_request, &WorkRunId(id("wrn", 10)))
        .unwrap();
    assert_eq!(
        pending.job.state,
        winwincode_storage::ExecutionJobState::Cancelling
    );
    assert_eq!(pending.job.revision, second.job.revision + 2);
    assert_eq!(
        pending.worker_session_id,
        Some(WorkerSessionId(id("wsn", 117)))
    );
    assert!(pending.codex_thread_id.is_none());
    assert!(pending.message_id.is_some());
    assert_eq!(
        storage
            .repository_scheduler()
            .unwrap()
            .list_jobs(
                &repository(),
                &[winwincode_storage::ExecutionJobState::Running]
            )
            .unwrap(),
        jobs_before
            .into_iter()
            .filter(|job| job.job_id == first.job.job_id)
            .collect::<Vec<_>>(),
        "pending cancellation must preserve the sibling running job"
    );
    drop(storage);
    let mut restarted = SqliteStorage::open(&root).unwrap();
    restarted
        .worker_session_slots()
        .unwrap()
        .open(&WorkerSlotOpenRequest {
            authority: WorkerSlotAuthority {
                worker_id: authority.lease().worker_id.clone(),
                worker_instance_id: authority.lease().worker_instance_id.clone(),
                worker_session_id: WorkerSessionId(id("wsn", 117)),
                codex_thread_id: CodexThreadId(id("cdx", 118)),
                job_id: second.job.job_id.clone(),
                lease_id: authority.lease().lease_id.clone(),
                attempt: authority.lease().attempt,
                fencing_token: authority.lease().fencing_token.clone(),
            },
            resources: WorkerSlotResources {
                memory_bytes: 1,
                disk_bytes: 1,
                process_slots: 1,
            },
            request_id: RequestId(id("req", 119)),
            opened_at: at(8),
        })
        .unwrap();

    let cancelled = restarted
        .repository_scheduler()
        .unwrap()
        .request_cancellation_for_work_run(&cancellation_request, &WorkRunId(id("wrn", 10)))
        .unwrap();
    assert!(cancelled.replayed);
    assert_eq!(cancelled.job.work_run_id, Some(WorkRunId(id("wrn", 10))));
    assert_eq!(
        cancelled.worker_session_id,
        Some(WorkerSessionId(id("wsn", 117)))
    );
    assert!(cancelled.message_id.is_some());
    assert_eq!(
        cancelled.codex_thread_id,
        Some(CodexThreadId(id("cdx", 118)))
    );
    let future_replay = restarted
        .repository_scheduler()
        .unwrap()
        .request_cancellation_for_work_run(&cancellation_request, &WorkRunId(id("wrn", 10)))
        .unwrap();
    assert_eq!(future_replay, cancelled);
    assert!(
        restarted
            .repository_scheduler()
            .unwrap()
            .request_cancellation_for_work_run(&cancellation_request, &WorkRunId(id("wrn", 9)),)
            .is_err(),
        "same cancellation request cannot replay under another WorkRun"
    );
    let wrong_work_run = restarted
        .repository_scheduler()
        .unwrap()
        .request_cancellation_for_work_run(
            &RepositorySchedulerCancellationRequest {
                request_id: RequestId(id("req", 121)),
                ..cancellation_request.clone()
            },
            &WorkRunId(id("wrn", 9)),
        )
        .expect_err("a different WorkRun cannot cancel this job");
    assert!(wrong_work_run.to_string().contains("WorkRun"));
    let running = restarted
        .repository_scheduler()
        .unwrap()
        .list_jobs(
            &repository(),
            &[winwincode_storage::ExecutionJobState::Running],
        )
        .unwrap();
    assert!(running.iter().any(|job| job.job_id == first.job.job_id));
    assert!(running.iter().all(|job| job.job_id != second.job.job_id));

    let first_cancellation = RepositorySchedulerCancellationRequest {
        scope: repository(),
        job_id: first.job.job_id.clone(),
        request_id: RequestId(id("req", 130)),
        expected_revision: first.job.revision + 1,
        requested_at: at(10),
    };
    let first_pending = restarted
        .repository_scheduler()
        .unwrap()
        .request_cancellation_for_work_run(&first_cancellation, &WorkRunId(id("wrn", 9)))
        .unwrap();
    assert!(first_pending.codex_thread_id.is_none());
    restarted
        .repository_scheduler()
        .unwrap()
        .settle_terminal(&RepositorySchedulerTerminalRequest {
            scope: repository(),
            terminal: ExecutionLeaseTerminalRequest {
                job_id: first.job.job_id.clone(),
                lease_id: first.lease.lease_id.clone(),
                worker_id: first.lease.worker_id.clone(),
                worker_instance_id: first.lease.worker_instance_id.clone(),
                attempt: first.lease.attempt,
                fencing_token: first.lease.fencing_token.clone(),
                outcome: ExecutionLeaseTerminalOutcome::Cancelled,
                terminal_at: at(11),
                request_id: RequestId(id("req", 131)),
            },
        })
        .unwrap();
    let stale_replay = restarted
        .repository_scheduler()
        .unwrap()
        .request_cancellation_for_work_run(&first_cancellation, &WorkRunId(id("wrn", 9)))
        .unwrap();
    assert!(stale_replay.replayed);
    assert!(stale_replay.codex_thread_id.is_none());
    assert_eq!(stale_replay.job, first_pending.job);

    let pending_jobs = restarted
        .repository_scheduler()
        .unwrap()
        .list_jobs(
            &repository(),
            &[
                winwincode_storage::ExecutionJobState::Running,
                winwincode_storage::ExecutionJobState::Cancelling,
            ],
        )
        .unwrap();
    assert!(pending_jobs.iter().any(|job| {
        job.job_id == second.job.job_id
            && job.state == winwincode_storage::ExecutionJobState::Cancelling
    }));
    fs::remove_dir_all(root).unwrap();
}
